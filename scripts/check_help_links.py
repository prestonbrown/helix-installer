#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Checks every helixscreen.org help link in the repository.

A help link is any https://helixscreen.org/... URL in a tracked text file except the installer's
own manifest tree (/installer/v1/), which is data the app fetches, not a page. Two checks:

  --scheme   offline: every link's path is a page address the site can serve
  (default)  live: every link answers 200 (HEAD, then GET when HEAD is refused; redirects followed),
             and a link with a #fragment names an id that exists in the page

  --pending FILE   live check only: paths listed in FILE (one per line, # comments) are expected
                   to be missing until the site deploys and are reported, not failed. The release
                   workflow passes no file, so a pending page blocks a release.
  --list           print the links and where they are used, then exit
"""
import argparse
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urldefrag, urlsplit

ROOT = Path(__file__).resolve().parent.parent
LINK = re.compile(r"https://helixscreen\.org(?:/[^\s\"'<>)\]\\`,;]*)?")
TRAILING = ".:!?"
NOT_HELP = ("/installer/v1/",)
# Pages the site serves: the docs root pages, the guides, reference, and the installer download
# page. A leading /1.1/ selects the beta docs tree.
PAGE = re.compile(
    r"^(?:/1\.1)?/(?:"
    r"|installation/|upgrading/|guide/"
    r"|guide/[a-z0-9-]+/"
    r"|guide/settings/[a-z0-9-]+/"
    r"|reference/[a-z0-9-]+/"
    r")$|^/(?:installer|flash|printers|whats-new|contact)/$"
)
ATTEMPTS = 3


def tracked_files():
    out = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT)
    # The tests carry made-up addresses on purpose.
    return [ROOT / p for p in out.decode().split("\0") if p and not p.endswith("test_check_help_links.py")]


def help_links(files=None):
    """{url, fragment included: [file:line, ...]} for every help link."""
    found = {}
    for path in files if files is not None else tracked_files():
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (UnicodeDecodeError, OSError):
            continue
        for n, line in enumerate(lines, 1):
            for m in LINK.finditer(line):
                url = m.group(0).rstrip(TRAILING)
                if any(urlsplit(url).path.startswith(p) for p in NOT_HELP):
                    continue
                if urlsplit(url).path == "":
                    url = url.replace("https://helixscreen.org", "https://helixscreen.org/", 1)
                where = f"{path.relative_to(ROOT)}:{n}" if path.is_relative_to(ROOT) else f"{path}:{n}"
                found.setdefault(url, []).append(where)
    return found


def scheme_problems(links):
    return {u: w for u, w in links.items() if not PAGE.match(urlsplit(u).path)}


def body(url):
    """The page's HTML, or None when it cannot be fetched."""
    for attempt in range(ATTEMPTS):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "helix-installer-link-check"})
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.read().decode("utf-8", "replace")
        except (urllib.error.URLError, TimeoutError, OSError):
            time.sleep(2 * (attempt + 1))
    return None


def has_anchor(html, fragment):
    """Whether the page carries an element with this id (Starlight gives every heading one)."""
    return f'id="{fragment}"' in html


def status(url):
    """HTTP status of url, following redirects; HEAD first, GET when HEAD does not answer 200."""
    last = None
    for attempt in range(ATTEMPTS):
        for method in ("HEAD", "GET"):
            req = urllib.request.Request(url, method=method, headers={"User-Agent": "helix-installer-link-check"})
            try:
                with urllib.request.urlopen(req, timeout=30) as r:
                    if r.status == 200:
                        return 200
                    last = r.status
            except urllib.error.HTTPError as e:
                last = e.code
            except (urllib.error.URLError, TimeoutError, OSError) as e:
                last = str(e)
        time.sleep(2 * (attempt + 1))
    return last


def read_pending(path):
    if not path:
        return set()
    lines = (l.split("#")[0].strip() for l in Path(path).read_text().splitlines())
    return {l for l in lines if l}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--scheme", action="store_true")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--pending")
    args = ap.parse_args(argv)

    links = help_links()
    if args.list:
        for url, where in sorted(links.items()):
            print(url, *where[:3], sep="\t")
        return 0
    if not links:
        print("no help links found: the extraction is broken", file=sys.stderr)
        return 1
    if args.scheme:
        bad = scheme_problems(links)
        for url, where in sorted(bad.items()):
            print(f"not a page address the site serves: {url}  ({', '.join(where[:3])})")
        print(f"{len(links)} help links, {len(bad)} off the site's scheme")
        return 1 if bad else 0

    pending = read_pending(args.pending)
    failed = []
    pages = {}
    for url in links:
        pages.setdefault(urldefrag(url)[0], []).append(url)
    for page in sorted(pages):
        path = urlsplit(page).path
        got = status(page)
        used = [w for u in pages[page] for w in links[u]]
        if got != 200:
            if path in pending:
                print(f"pending  {page}  ({got})")
            else:
                print(f"FAIL     {page}  ({got})  used in {', '.join(used[:3])}")
                failed.append(page)
            continue
        print(f"ok       {page}")
        fragments = sorted({urldefrag(u)[1] for u in pages[page] if urldefrag(u)[1]})
        if not fragments:
            continue
        html = body(page)
        for frag in fragments:
            if html is not None and has_anchor(html, frag):
                print(f"ok       {page}#{frag}")
            else:
                where = [w for u in pages[page] if urldefrag(u)[1] == frag for w in links[u]]
                print(f"FAIL     {page}#{frag}  no such anchor  used in {', '.join(where[:3])}")
                failed.append(f"{page}#{frag}")
    print(f"{len(links)} help links, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
