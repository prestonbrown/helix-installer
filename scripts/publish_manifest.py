#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Builds manifest/manifest.json, the published printer list, from manifest/staging.json.

Staging holds every printer and recipe. manifest/publish.json names what is published:

  {"printers": {
      "<printer id>": {
          "recipes": ["<recipe>", ...],        staging choices naming these are kept
          "route": {"<recipe>": "<recipe>"},   a staging choice naming the key is kept with the
                                               value as its recipe; the value must be published
          "verified": {...}                    the printer's verified record: one run per
                                               hardware run, each with the recipe_sha256 it ran
                                               (manifest-lint --digests), and awaiting_rerun for
                                               offered recipes changed since
      },
      "<printer id>": {"refuse": true},        published as refused: identified, installs nothing
      "<refused staging printer>": {}          copied as it is
  },
  "notes": ["..."]}                            for the people editing it; not published

Every other staging choice is dropped, so identification refuses that state with the printer's
guide. Recipes are copied from staging unchanged, except that a step image that is still an empty
placeholder under manifest/img is left out (the text stays); a missing image is an error. Only recipes and artifacts a
published printer reaches are kept; printers keep staging's order.

  publish_manifest.py           write manifest/manifest.json
  publish_manifest.py --check   exit 1 when manifest/manifest.json is not what staging builds
"""
import argparse
import copy
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC_KEYS = {"recipes", "route", "verified", "refuse"}
TOP_KEYS = {"printers", "notes"}


class SpecError(Exception):
    pass


def usable_image(img_root, rel):
    """False for an empty placeholder; a missing file is an error."""
    p = img_root / rel
    if not p.is_file():
        raise SpecError(f"image {rel} is not under {img_root}")
    return p.stat().st_size > 0


def strip_images(steps, img_root):
    for s in steps:
        if "image" in s and not usable_image(img_root, s["image"]):
            del s["image"]
        for c in s.get("choices", []):
            if "image" in c and not usable_image(img_root, c["image"]):
                del c["image"]
    return steps


def restore_recipes(restore):
    if restore is None:
        return []
    if isinstance(restore, str):
        return [restore]
    return [c["recipe"] for c in restore]


def step_artifacts(s):
    if s.get("type") == "download":
        return [s["artifact"]]
    if s.get("type") == "write_stick":
        return list(s.get("artifacts", [])) + list(s.get("rename", {}))
    return []


def publish_printer(p, sel):
    unknown = set(sel) - SPEC_KEYS
    if unknown:
        raise SpecError(f"{p['id']}: unknown keys {sorted(unknown)}")
    if p.get("refused"):
        if sel:
            raise SpecError(f"{p['id']} is refused in staging; publish it with {{}}")
        return copy.deepcopy(p)
    if sel.get("refuse"):
        if set(sel) != {"refuse"}:
            raise SpecError(f"{p['id']}: a printer published as refused takes nothing else")
        out = copy.deepcopy(p)
        out["refused"] = True
        out["recipes"] = []
        out.pop("restore", None)
        out.pop("verified", None)
        return out
    keep = sel.get("recipes", [])
    route = sel.get("route", {})
    if not keep:
        raise SpecError(f"{p['id']}: name the recipes to publish, or publish it refused")
    if "verified" not in sel:
        raise SpecError(f"{p['id']}: a published printer needs its verified record")
    named = {c["recipe"] for c in p["recipes"]}
    for r in keep:
        if r not in named:
            raise SpecError(f"{p['id']}: no staging choice names {r}")
    for src, dst in route.items():
        if src not in named:
            raise SpecError(f"{p['id']}: no staging choice names {src} to route")
        if dst not in keep:
            raise SpecError(f"{p['id']}: {src} routes to {dst}, which is not published")
    out = copy.deepcopy(p)
    out["recipes"] = []
    for c in p["recipes"]:
        if c["recipe"] in keep:
            out["recipes"].append(copy.deepcopy(c))
        elif c["recipe"] in route:
            out["recipes"].append({"when": copy.deepcopy(c["when"]), "recipe": route[c["recipe"]]})
    out["verified"] = copy.deepcopy(sel["verified"])
    return out


def build(staging, spec, img_root):
    """The published manifest for `spec`, built from `staging` (left unchanged)."""
    unknown = set(spec) - TOP_KEYS
    if unknown:
        raise SpecError(f"unknown keys {sorted(unknown)}")
    sel = spec.get("printers", {})
    known = {p["id"] for p in staging["printers"]}
    for pid in sel:
        if pid not in known:
            raise SpecError(f"{pid} is not a staging printer")
    printers = [publish_printer(p, sel[p["id"]]) for p in staging["printers"] if p["id"] in sel]
    reached = []
    for p in printers:
        for r in [c["recipe"] for c in p["recipes"]] + restore_recipes(p.get("restore")):
            if r not in reached:
                reached.append(r)
    recipes = {
        rid: strip_images(copy.deepcopy(steps), img_root)
        for rid, steps in staging["recipes"].items()
        if rid in reached
    }
    used = {a for steps in recipes.values() for s in steps for a in step_artifacts(s)}
    artifacts = {k: copy.deepcopy(v) for k, v in staging["artifacts"].items() if k in used}
    return {
        "manifest_version": staging["manifest_version"],
        "min_app_version": staging["min_app_version"],
        "printers": printers,
        "artifacts": artifacts,
        "recipes": recipes,
    }


def render(m):
    return json.dumps(m, indent=2, ensure_ascii=False) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--staging", default=str(ROOT / "manifest/staging.json"))
    ap.add_argument("--spec", default=str(ROOT / "manifest/publish.json"))
    ap.add_argument("--img", default=str(ROOT / "manifest/img"))
    ap.add_argument("--out", default=str(ROOT / "manifest/manifest.json"))
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args(argv)

    staging = json.loads(Path(args.staging).read_text(encoding="utf-8"))
    spec = json.loads(Path(args.spec).read_text(encoding="utf-8"))
    try:
        text = render(build(staging, spec, Path(args.img)))
    except SpecError as e:
        print(f"publish.json: {e}", file=sys.stderr)
        return 2
    out = Path(args.out)
    if args.check:
        current = out.read_text(encoding="utf-8") if out.is_file() else ""
        if current != text:
            print(
                f"{out} is not what staging.json and publish.json build; "
                "run scripts/publish_manifest.py and commit the result",
                file=sys.stderr,
            )
            return 1
        return 0
    out.write_text(text, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
