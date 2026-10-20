#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Tests for check_help_links.py: what counts as a help link and which addresses the site serves."""
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_help_links as c  # noqa: E402


def links_in(text):
    d = Path(tempfile.mkdtemp())
    p = d / "x.txt"
    p.write_text(text)
    with mock.patch.object(c, "ROOT", d):
        return c.help_links([p])


class Extraction(unittest.TestCase):
    def test_finds_links_in_prose_and_json_and_drops_trailing_punctuation(self):
        got = links_in(
            'follow the guide: https://helixscreen.org/guide/install-k1/. Or "https://helixscreen.org/installation/"'
        )
        self.assertEqual(
            set(got), {"https://helixscreen.org/guide/install-k1/", "https://helixscreen.org/installation/"}
        )

    def test_fragment_is_kept_so_the_anchor_can_be_checked(self):
        got = links_in("https://helixscreen.org/guide/install-generic/#the-sudo-password")
        self.assertEqual(set(got), {"https://helixscreen.org/guide/install-generic/#the-sudo-password"})

    def test_manifest_tree_and_other_hosts_are_not_help_links(self):
        got = links_in(
            "https://helixscreen.org/installer/v1/manifest.json https://releases.helixscreen.org/install.sh"
        )
        self.assertEqual(got, {})

    def test_bare_host_is_the_site_root(self):
        self.assertEqual(set(links_in("see https://helixscreen.org")), {"https://helixscreen.org/"})


class Scheme(unittest.TestCase):
    def problems(self, *paths):
        return set(c.scheme_problems({f"https://helixscreen.org{p}": ["x:1"] for p in paths}))

    def test_site_pages_pass(self):
        self.assertEqual(
            self.problems("/installation/", "/guide/install-k1/", "/1.1/guide/install-esp32/", "/installer/", "/"), set()
        )

    def test_the_retired_docs_prefix_and_pages_without_a_trailing_slash_fail(self):
        bad = self.problems("/docs/install-k1", "/guide/install-k1", "/guide/Install_K1/")
        self.assertEqual(len(bad), 3)


class Anchors(unittest.TestCase):
    PAGE = '<h2 id="the-sudo-password">The sudo Password</h2>'

    def test_a_heading_id_is_found(self):
        self.assertTrue(c.has_anchor(self.PAGE, "the-sudo-password"))

    def test_a_missing_anchor_is_not(self):
        self.assertFalse(c.has_anchor(self.PAGE, "the-sudo-passwords"))

    def test_live_check_fails_on_a_missing_anchor_and_passes_on_a_present_one(self):
        links = {
            "https://helixscreen.org/guide/x/#here": ["a:1"],
            "https://helixscreen.org/guide/x/#gone": ["a:2"],
        }
        with mock.patch.object(c, "help_links", return_value=links), mock.patch.object(
            c, "status", return_value=200
        ), mock.patch.object(c, "body", return_value='<h2 id="here">x</h2>'):
            self.assertEqual(c.main([]), 1)
        with mock.patch.object(c, "help_links", return_value={k: v for k, v in links.items() if "here" in k}), mock.patch.object(
            c, "status", return_value=200
        ), mock.patch.object(c, "body", return_value='<h2 id="here">x</h2>'):
            self.assertEqual(c.main([]), 0)


class Pending(unittest.TestCase):
    def test_pending_file_ignores_comments_and_blank_lines(self):
        d = Path(tempfile.mkdtemp()) / "pending.txt"
        d.write_text("# not yet deployed\n/guide/install-k1/  # K1 page\n\n")
        self.assertEqual(c.read_pending(str(d)), {"/guide/install-k1/"})


if __name__ == "__main__":
    unittest.main()
