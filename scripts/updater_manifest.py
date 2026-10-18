#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build the Tauri updater manifest (latest.json) from the release bundles.

usage: updater_manifest.py <bundles-dir> <base-url> <out-dir>

Every updater bundle found under <bundles-dir> (with its .sig) is copied flat into <out-dir>,
and <out-dir>/latest.json points each platform at <base-url>/<version>/<file>.
"""
import json
import re
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path

# platform key -> (suffix the updater bundle ends with, extra requirement on its name)
PLATFORMS = {
    "windows-x86_64": ("-setup.exe", None),
    "linux-x86_64": (".AppImage", "x86_64|amd64"),
    "darwin-x86_64": (".app.tar.gz", None),
    "darwin-aarch64": (".app.tar.gz", None),
}


def find_bundle(files, suffix, name_re):
    hits = [
        f
        for f in files
        if f.name.endswith(suffix)
        and Path(str(f) + ".sig") in files
        and (name_re is None or re.search(name_re, f.name))
    ]
    if len(hits) > 1:
        raise SystemExit(f"more than one {suffix} bundle: {[h.name for h in hits]}")
    return hits[0] if hits else None


def build(bundles: Path, base_url: str, out: Path):
    files = {p for p in bundles.rglob("*") if p.is_file()}
    version = None
    platforms = {}
    for key, (suffix, name_re) in PLATFORMS.items():
        bundle = find_bundle(files, suffix, name_re)
        if bundle is None:
            continue
        m = re.search(r"_(\d+\.\d+\.\d+[^_]*?)_", bundle.name)
        if m is None:
            raise SystemExit(f"no version in {bundle.name}")
        if version not in (None, m.group(1)):
            raise SystemExit(f"bundles disagree on the version: {version} and {m.group(1)}")
        version = m.group(1)
        sig = Path(str(bundle) + ".sig").read_text().strip()
        platforms[key] = (bundle, sig)
    if not platforms:
        raise SystemExit("no signed updater bundles found")
    out.mkdir(parents=True, exist_ok=True)
    manifest = {
        "version": version,
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {},
    }
    for key, (bundle, sig) in platforms.items():
        shutil.copy(bundle, out / bundle.name)
        shutil.copy(str(bundle) + ".sig", out / (bundle.name + ".sig"))
        manifest["platforms"][key] = {
            "signature": sig,
            "url": f"{base_url.rstrip('/')}/{version}/{bundle.name}",
        }
    (out / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    build(Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))
