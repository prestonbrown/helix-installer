---
description: Release the HelixScreen easy installer app: version bump, signing check, tag, draft build, smoke test, publish.
argument-hint: "[patch | minor | major | X.Y.Z]"
allowed-tools: ["Bash", "Read", "Edit", "Grep", "AskUserQuestion"]
---

# Release the easy installer

This releases the desktop app (`v*` tag -> `.github/workflows/release.yml` -> a draft GitHub
release with the macOS, Windows and Linux builds). It is not the HelixScreen release; that is
helixscreen's own `/release`. The printer list (`manifest/manifest.json`) publishes separately,
when `main` moves (`docs/SIGNING.md`).

Stop at the first failure. Never push to `main` or tag without Preston's go in this session.

## 1. Gather

- `git fetch --tags origin`; the release is cut from `origin/main`. It must contain `origin/engine`
  (`git merge-base --is-ancestor origin/engine origin/main`), or ask whether to merge engine first.
- Latest tag: `git tag -l 'v*' --sort=-v:refname | head -1`. Version argument: `patch`, `minor`,
  `major` or an exact `X.Y.Z`; none means `patch`.
- CI on the `origin/main` head must be green for both workflows:
  `gh run list -b main -L 4 --json workflowName,headSha,conclusion`. Red or running: stop.

## 2. Signing check (ask about every missing piece)

Read what is set, never the values:

```bash
R=prestonbrown/helix-installer
gh secret list -R $R                         # repository secrets
gh secret list -R $R --env manifest-publish  # environment secrets
gh variable list -R $R
grep -n 'pub const .*PUBKEY' crates/core/src/keys.rs
```

Build this table and show it:

| What | Needs | Effect when missing |
|---|---|---|
| Printer-list key | `MANIFEST_PUBKEY` non-empty in keys.rs; env secrets `MANIFEST_SIGNING_KEY`, `MANIFEST_SIGNING_KEY_PASSWORD`, `R2_*`; variable `MANIFEST_PUBLISH_ENABLED=true` | **Blocking.** The app trusts no printer list, so it can do nothing. Stop and walk Preston through `docs/SIGNING.md` steps 1-5. |
| In-app updater | `UPDATER_PUBKEY` non-empty in keys.rs; repo secrets `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Installed copies never update themselves; users download each release by hand. |
| macOS | repo secrets `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` (sign), plus `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` (notarize) | Gatekeeper blocks the .dmg until the user uses Privacy & Security > Open Anyway. |
| Windows | repo secrets `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE` | SmartScreen warns until the user picks More info > Run anyway. |

Then, for each row that is not fully set, ask Preston with **AskUserQuestion** (one round, one
question per missing row): "Set it up now" (stop here and walk `docs/SIGNING.md` for that row:
the commands that need a secret are his to run, never paste a secret into this session) or
"Ship without it" (record it for the release notes). A partly set row (some of a group's secrets
but not all) is always a question: the workflow treats it as missing.

Two rules that are not choices:
- The updater key, once compiled in, is permanent: a new key strands every installed copy.
  Adding `UPDATER_PUBKEY` is a code change on its own commit, through CI, before the tag.
- Approving the `manifest-publish` deployment of `publish-updater` ships the update to every
  installed copy within minutes, even while the GitHub release is a draft.

## 3. Version

The version lives in four places and the tag must name it (the release workflow refuses a tag that
does not match `crates/app/tauri.conf.json`):

- `crates/app/tauri.conf.json` `"version"`
- `ui/package.json` `"version"` (and `ui/package-lock.json`'s two top entries)
- `crates/app/Cargo.toml` and `crates/core/Cargo.toml` `version`, then `cargo update -w` so
  `Cargo.lock` follows

Bump all of them on a branch `release/vX.Y.Z` from `origin/main`, commit
`release: vX.Y.Z`, push the branch, wait for CI green (`gh run watch <id> --exit-status`), then
fast-forward `main` to it (Preston's go) and push.

## 4. Gates the workflow also runs (run them first, they are cheap)

```bash
python3 scripts/check_help_links.py                       # every help page live; no pending list
python3 scripts/publish_manifest.py --check               # published list matches staging
cargo run -q -p helix_installer_core --bin manifest-lint -- manifest/manifest.json manifest --signing
```

Any failure: stop. A non-empty `scripts/pending-help-pages.txt` means a linked page is not live yet.

## 5. Tag and build

```bash
git tag -a vX.Y.Z -m "HelixScreen Installer vX.Y.Z" <release commit> && git push origin vX.Y.Z
```

`v*` tags are protected by a ruleset (admins only), so the tag must be pushed as Preston's account.
Watch the run: `gh run list -w release.yml -L 1`, then `gh run watch <id> --exit-status`. Read the
job summaries: each build job lists exactly what came out signed and what did not. It must match
the step 2 answers; a mismatch (a secret thought set but reported missing) is a stop.

If `publish-updater` waits on the `manifest-publish` environment, do NOT ask Preston to approve it
yet: that ships the update. It is approved in step 7.

## 6. Smoke test (Preston)

Give Preston the draft release URL (`gh release view vX.Y.Z --json url`) and this checklist:

- macOS: download the `.dmg`, open it (Open Anyway if unsigned), run discovery, pick a printer,
  stop at the login screen.
- Windows: run the `.exe` or `.msi` (More info > Run anyway if unsigned), same steps.
- Linux: the AppImage, same steps (thelio can run it headless with `e2e/run-xvfb.sh` instead).
- On one machine: the version in the bottom-left corner reads X.Y.Z.

Wait for his result. Any failure: stop and leave the release as a draft.

## 7. Publish

On his go:

```bash
gh release edit vX.Y.Z --draft=false
```

Then, if the updater is set up, Preston approves the `publish-updater` deployment in GitHub
(Actions > the release run > Review deployments). Check
`https://releases.helixscreen.org/installer/app/latest.json` names X.Y.Z.

Release notes name every row from step 2 that shipped without signing, and how users get past it
(the download page at https://helixscreen.org/installer/ explains each).
