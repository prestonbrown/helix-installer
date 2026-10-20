#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Tests for publish_manifest.py: the published list is built from staging, never written by hand."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import publish_manifest as pm  # noqa: E402

VERIFIED = {"by": "helixscreen", "date": "2026-10-19", "firmware": "1.3.3.46"}


def staging():
    return {
        "manifest_version": 1,
        "min_app_version": "0.1.0",
        "printers": [
            {
                "id": "k1",
                "name": "K1",
                "identify": [{"cpu_arch": "mips"}],
                "recipes": [
                    {"when": {"state": "helix"}, "recipe": "k1_update"},
                    {"when": {"state": "modded:moonraker"}, "recipe": "k1_install"},
                    {"when": {"state": "stock"}, "recipe": "k1_stock"},
                ],
                "restore": "k1_restore",
                "fallback": {"refuse": True, "doc": "https://helixscreen.org/guide/install-k1/"},
            },
            {
                "id": "ad5x",
                "name": "AD5X",
                "identify": [{"model": "AD5X"}],
                "recipes": [{"when": {"state": "stock"}, "recipe": "ad5x_zmod"}],
                "restore": "ad5x_restore",
                "fallback": {"refuse": True, "doc": "https://helixscreen.org/guide/install-ad5x/"},
            },
            {
                "id": "sovol",
                "name": "Sovol",
                "refused": True,
                "identify": [{"model": "sv08"}],
                "recipes": [],
                "fallback": {"refuse": True, "doc": "https://helixscreen.org/guide/easy-installer-unsupported/"},
            },
        ],
        "artifacts": {
            "simple_af": {"url": "https://example.org/af.tgz", "sha256": "a" * 64},
            "zmod": {"url": "https://example.org/zmod.bin", "sha256": "b" * 64},
        },
        "recipes": {
            "k1_update": [
                {"type": "user_action", "text": "Turn on root", "image": "k1/root.png"},
                {"type": "info", "text": "Here it is", "image": "k1/real.png"},
                {"type": "wait_for", "target": "helix_service", "timeout_s": 5, "message": "m"},
            ],
            "k1_install": [{"type": "download", "artifact": "simple_af"}],
            "k1_stock": [{"type": "download", "artifact": "simple_af"}],
            "k1_restore": [{"type": "ssh_run", "command": "true", "timeout_s": 5, "commits": True}],
            "ad5x_zmod": [{"type": "write_stick", "artifacts": ["zmod"]}],
            "ad5x_restore": [{"type": "ssh_run", "command": "true", "timeout_s": 5, "commits": True}],
        },
    }


def images():
    d = Path(tempfile.mkdtemp())
    (d / "k1").mkdir()
    (d / "k1" / "root.png").write_bytes(b"")
    (d / "k1" / "real.png").write_bytes(b"\x89PNG")
    return d


def spec(**printers):
    return {"printers": printers}


class Build(unittest.TestCase):
    def build(self, s):
        return pm.build(staging(), s, images())

    def test_only_the_listed_printers_and_choices_are_published(self):
        m = self.build(spec(k1={"recipes": ["k1_update"], "verified": VERIFIED}))
        self.assertEqual([p["id"] for p in m["printers"]], ["k1"])
        k1 = m["printers"][0]
        self.assertEqual(k1["recipes"], [{"when": {"state": "helix"}, "recipe": "k1_update"}])
        self.assertEqual(k1["verified"], VERIFIED)
        self.assertEqual(set(m["recipes"]), {"k1_update", "k1_restore"})
        self.assertEqual(m["artifacts"], {})

    def test_a_routed_choice_keeps_its_when_and_names_the_published_recipe(self):
        m = self.build(
            spec(k1={"recipes": ["k1_update"], "route": {"k1_install": "k1_update"}, "verified": VERIFIED})
        )
        self.assertEqual(
            m["printers"][0]["recipes"],
            [
                {"when": {"state": "helix"}, "recipe": "k1_update"},
                {"when": {"state": "modded:moonraker"}, "recipe": "k1_update"},
            ],
        )
        self.assertNotIn("k1_install", m["recipes"])

    def test_an_empty_placeholder_image_is_dropped_and_the_text_kept(self):
        m = self.build(spec(k1={"recipes": ["k1_update"], "verified": VERIFIED}))
        steps = m["recipes"]["k1_update"]
        self.assertEqual(steps[0], {"type": "user_action", "text": "Turn on root"})
        self.assertEqual(steps[1]["image"], "k1/real.png")

    def test_a_missing_image_is_an_error_not_dropped(self):
        img = images()
        (img / "k1" / "real.png").unlink()
        with self.assertRaises(pm.SpecError):
            pm.build(staging(), spec(k1={"recipes": ["k1_update"], "verified": VERIFIED}), img)

    def test_notes_are_allowed_and_not_published(self):
        s = spec(k1={"recipes": ["k1_update"], "verified": VERIFIED})
        s["notes"] = ["restores are published unverified"]
        self.assertNotIn("notes", self.build(s))

    def test_staging_is_left_as_it_was(self):
        st = staging()
        pm.build(st, spec(k1={"recipes": ["k1_update"], "verified": VERIFIED}), images())
        self.assertEqual(st, staging())

    def test_a_printer_published_as_refused_installs_nothing(self):
        m = self.build(spec(ad5x={"refuse": True}))
        ad5x = m["printers"][0]
        self.assertTrue(ad5x["refused"])
        self.assertEqual(ad5x["recipes"], [])
        self.assertNotIn("restore", ad5x)
        self.assertEqual(ad5x["fallback"]["doc"], "https://helixscreen.org/guide/install-ad5x/")
        self.assertEqual(m["recipes"], {})
        self.assertEqual(m["artifacts"], {})

    def test_a_refused_staging_printer_is_copied_as_it_is(self):
        m = self.build(spec(sovol={}))
        self.assertEqual(m["printers"], [staging()["printers"][2]])

    def test_printers_keep_stagings_order(self):
        m = self.build(spec(sovol={}, k1={"recipes": ["k1_update"], "verified": VERIFIED}))
        self.assertEqual([p["id"] for p in m["printers"]], ["k1", "sovol"])

    def test_mistakes_in_the_selection_are_errors(self):
        for bad in [
            spec(nope={}),
            spec(k1={"recipes": ["k1_upgrade"], "verified": VERIFIED}),
            spec(k1={"recipes": ["k1_update"], "route": {"k1_nope": "k1_update"}, "verified": VERIFIED}),
            spec(k1={"recipes": ["k1_update"], "route": {"k1_install": "k1_stock"}, "verified": VERIFIED}),
            spec(k1={"verified": VERIFIED}),
            spec(k1={"recipes": ["k1_update"]}),
            spec(sovol={"recipes": ["x"]}),
            spec(k1={"recipes": ["k1_update"], "verified": VERIFIED, "typo": 1}),
            {"printers": {}, "typo": 1},
        ]:
            with self.subTest(bad=bad), self.assertRaises(pm.SpecError):
                pm.build(staging(), bad, images())


class Check(unittest.TestCase):
    def tree(self):
        d = Path(tempfile.mkdtemp())
        (d / "img" / "k1").mkdir(parents=True)
        (d / "img" / "k1" / "real.png").write_bytes(b"\x89PNG")
        (d / "img" / "k1" / "root.png").write_bytes(b"")
        (d / "staging.json").write_text(json.dumps(staging()))
        (d / "publish.json").write_text(json.dumps(spec(k1={"recipes": ["k1_update"], "verified": VERIFIED})))
        return d

    def run_main(self, d, *extra):
        return pm.main(
            ["--staging", str(d / "staging.json"), "--spec", str(d / "publish.json"),
             "--img", str(d / "img"), "--out", str(d / "manifest.json"), *extra]
        )

    def test_check_passes_on_a_fresh_build_and_fails_once_staging_moves(self):
        d = self.tree()
        self.assertEqual(self.run_main(d), 0)
        self.assertEqual(self.run_main(d, "--check"), 0)
        st = staging()
        st["recipes"]["k1_update"][0]["text"] = "Turn on root access"
        (d / "staging.json").write_text(json.dumps(st))
        self.assertEqual(self.run_main(d, "--check"), 1)

    def test_check_fails_on_a_hand_edit_of_the_published_file(self):
        d = self.tree()
        self.run_main(d)
        out = d / "manifest.json"
        out.write_text(out.read_text().replace("Here it is", "Here it was"))
        self.assertEqual(self.run_main(d, "--check"), 1)


if __name__ == "__main__":
    unittest.main()
