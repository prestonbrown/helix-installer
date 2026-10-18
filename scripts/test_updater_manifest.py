#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Tests for updater_manifest.py, on bundles named the way the Tauri bundler names them."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import updater_manifest  # noqa: E402

BASE = "https://releases.helixscreen.org/installer/app"


def bundles(root: Path, names):
    for rel in names:
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(b"bundle " + rel.encode())
        Path(str(p) + ".sig").write_text(f"sig of {p.name}\n")


class UpdaterManifest(unittest.TestCase):
    def build(self, names, version="0.1.0"):
        d = Path(tempfile.mkdtemp())
        bundles(d / "in", names)
        manifest = updater_manifest.build(d / "in", BASE, d / "out", version)
        return manifest, d / "out"

    def test_the_macos_tarball_without_a_version_is_published_under_a_versioned_name(self):
        manifest, out = self.build(["macos/HelixScreen Installer.app.tar.gz"])
        self.assertEqual(manifest["version"], "0.1.0")
        url = manifest["platforms"]["darwin-aarch64"]["url"]
        self.assertEqual(url, f"{BASE}/0.1.0/HelixScreen_Installer_0.1.0.app.tar.gz")
        self.assertEqual(manifest["platforms"]["darwin-x86_64"]["url"], url)
        self.assertTrue((out / "HelixScreen_Installer_0.1.0.app.tar.gz").is_file())
        self.assertTrue((out / "HelixScreen_Installer_0.1.0.app.tar.gz.sig").is_file())
        self.assertEqual(
            manifest["platforms"]["darwin-aarch64"]["signature"],
            "sig of HelixScreen Installer.app.tar.gz",
        )

    def test_every_platform_gets_a_url_without_spaces(self):
        manifest, out = self.build(
            [
                "windows/HelixScreen Installer_0.1.0_x64-setup.exe",
                "linux/HelixScreen Installer_0.1.0_amd64.AppImage",
                "macos/HelixScreen Installer.app.tar.gz",
            ]
        )
        urls = [p["url"] for p in manifest["platforms"].values()]
        self.assertEqual(len(urls), 4)
        for url in urls:
            self.assertNotIn(" ", url)
            self.assertTrue(url.startswith(f"{BASE}/0.1.0/"), url)
        self.assertEqual(
            manifest["platforms"]["windows-x86_64"]["url"],
            f"{BASE}/0.1.0/HelixScreen_Installer_0.1.0_x64-setup.exe",
        )
        written = json.loads((out / "latest.json").read_text())
        self.assertEqual(written, manifest)

    def test_a_bundle_named_for_another_version_is_refused(self):
        with self.assertRaises(SystemExit):
            self.build(["windows/HelixScreen Installer_0.0.9_x64-setup.exe"])

    def test_no_signed_bundle_is_an_error(self):
        d = Path(tempfile.mkdtemp())
        (d / "in").mkdir()
        (d / "in" / "HelixScreen Installer.app.tar.gz").write_bytes(b"unsigned")
        with self.assertRaises(SystemExit):
            updater_manifest.build(d / "in", BASE, d / "out", "0.1.0")


if __name__ == "__main__":
    unittest.main()
