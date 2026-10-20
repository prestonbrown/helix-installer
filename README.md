# HelixScreen Installer

A desktop app that installs [HelixScreen](https://github.com/prestonbrown/helixscreen) onto 3D printers for people who would rather not use a terminal. Users download it from [helixscreen.org/installer](https://helixscreen.org/installer/). This file is for people changing it.

The installer logic lives in `crates/core` (pure Rust, no Tauri), the Tauri 2 shell in `crates/app`, and the Svelte 5 + Vite + TypeScript interface in `ui/`. `docs/devel/INSTALLER.md` in the main repository describes how the app drives `install.sh`.

## Build and run

```sh
npm ci --prefix ui
ui/node_modules/.bin/tauri dev     # or `cargo tauri dev` with `cargo install tauri-cli`
```

From the repository root: the CLI finds `crates/app` and runs the UI's own scripts in `ui/`. Linux needs the webkit2gtk 4.1 development package (`libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, `patchelf` on Debian and Ubuntu). A release build is `tauri build` the same way; the release workflow does it for every OS when a `v*` tag is pushed and leaves a draft release for a person to publish.

The printer list is a signed manifest. Until one is published, point the app at one with `HELIX_INSTALLER_MANIFEST` (a URL or file path; its signature sits beside it as `<location>.minisig`) and `HELIX_INSTALLER_PUBKEY` (the minisign public key, or a path to the `.pub` file; honored only by debug builds such as `cargo tauri dev`). Without a list that verifies, the app shows "Can't load the printer list". `HELIX_INSTALLER_CHANNEL=stable|beta` makes every `helix_install` step read that HelixScreen release channel instead of the recipe's.

To see the interface without the Rust side, `npm run dev:mock` in `ui/` serves it against a mock of every command; `?screen=info` (or `user_action`, `ask`, `confirm`, `warn`, `login`, `drive`, `drive_empty`, `progress`, `done`, `refused`, `failed`, `recovery`, `resume`, `no_list`) picks what an install shows after the first printer's button. `node scripts/shots.mjs <dir>` (in `ui/`) captures every screen headless, light and dark, at 1280x800 and 1024x640.

`e2e/run-xvfb.sh` drives a debug build (`cargo tauri build --debug --no-bundle`) through one update run on a real printer, on a private Xvfb display, over WebDriver: it needs `cargo install tauri-driver` and `WebKitWebDriver` (the `webkit2gtk-driver` package). The variables it reads are listed at the top of `e2e/run-update.mjs`; the password is read from the variable `E2E_PW_VAR` names and never printed. It stops at any screen it does not know how to answer, so it never agrees to a change on the printer's behalf. It is not part of CI.

## Test

Run these before every commit; CI runs them on Linux, macOS and Windows.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd ui && npm run check && npm test)
python3 scripts/test_updater_manifest.py
python3 scripts/test_check_help_links.py
```

`cargo` here may be a jobpool shim: never pass `-j`.

### The fakes

No test touches a printer, the network or a drive. Every outside dependency is a trait in `crates/core/src/transport/mod.rs` with a scriptable in-memory fake in `transport/fake.rs`, bundled into an `Env`:

| Fake | Stands in for |
|------|---------------|
| `FakeSsh` | The printer's SSH server: scripted answers per command, uploads recorded, scripted detached jobs such as an `install.sh` run printing `HELIX-PROGRESS` lines |
| `FakeMoonraker` | Moonraker's HTTP API per address: `printer_info`, `system_info`, object lists, G-code sent, restart timing |
| `FakeFetch` | HTTP downloads (release lists, `install.sh`, firmware), keyed by URL |
| `FakeDrives` | The drives the PC offers for a USB stick |
| `FakeSerial` | An ESP32 on a serial port, with a flash session |
| `FakeNet` | TCP ports and mDNS |
| `FakeClock` | Time, so a 15-minute wait costs nothing |

`crates/core/tests/ssh_server` is an in-process SSH server for the tests of the russh transport itself: it answers the command shapes the transport sends and records every command and every byte fed to sudo. Printers are recorded as JSON in `crates/core/tests/fixtures/probes` (what identification sees) and `fixtures/fleet` (facts from real units); `tests/recipes.rs` runs every recipe in `manifest/staging.json` end to end against the fakes from those facts.

A test must fail when the feature is removed: break the line it pins once and watch it go red before you trust it.

## Recipes and the manifest

`manifest/manifest.json` is the signed list the app ships against. `manifest/staging.json` holds every printer and recipe, run on hardware or not: CI lints it like the published list but never signs or uploads it. `manifest.json` is never edited by hand: `scripts/publish_manifest.py` builds it from staging and `manifest/publish.json`, which names each published printer, the recipes it offers (a starting state whose recipe is not published refuses with the printer's guide, or `route` sends it to a published recipe that probes first), and its `verified` record with one entry per hardware run. Recipe text is copied from staging unchanged except that a step picture still a 0-byte placeholder is left out. CI fails when `manifest.json` is not what the script builds (`--check`), and the strict lint refuses an empty picture. `manifest/img` holds the pictures recipe steps show.

A manifest has:

- `printers[]`: `id`, `name`, `photo`, `identify` (rules), `recipes` (which recipe for which starting state), `restore`, `fallback` (`refuse: true` and a help `doc`), an optional `verified` (`by`, `date`, `firmware`), `refused` (the printer is recognized and turned away) and `last_resort` (matches only when nothing else does).
- `recipes`: a name to a list of steps.
- `artifacts`: named downloads with an https `url` and a `sha256`. A file is never written whose hash is not in the signed manifest, or for a HelixScreen release in its release list.

**Identification.** Each `identify` rule is a set of facts that must all match; a printer matches when any rule does. A condition is an exact value, `{"contains": x}` or `{"contains_all": [x, y]}` for a list fact, `{"prefix": x}`, `{"not": x}` or a version range. A missing fact never matches. Facts come from discovery (mDNS, the FlashForge UDP reply, a USB id, Moonraker's `printer_info`, `objects_list`) and from `probe` steps.

**Starting state.** Identification adds a `state` fact: `helix` (HelixScreen is installed), `modded:moonraker` (Moonraker answers), or `stock`. A printer's `recipes` list is walked in order and the first entry whose `when` matches wins, so put the narrow ones first.

**Steps.** A recipe is a list of these, run in order by `crates/core/src/runner.rs`:

| `type` | What it does |
|--------|--------------|
| `info`, `user_action` | Shows text (and a picture); `user_action` can `verify` with a probe before Next |
| `ask` | Asks the user a multiple-choice question and stores the answer as a fact |
| `probe` | Reads a fact over Moonraker, SSH, a TCP port or a serial port |
| `require` | Stops with a `reason` and a help `doc` unless a fact equals a value or is in a version range; with `warn: true` it shows them instead and the user continues or stops |
| `download`, `write_stick` | Fetches artifacts, then writes them to a FAT32 removable drive (never formats) |
| `wait_for` | Waits for Moonraker, Klipper, an SSH login, a restart, or the HelixScreen service |
| `ssh_login` | Tries each listed login, then asks the user; keys from the agent and `~/.ssh` are offered too |
| `ssh_run`, `moonraker_gcode`, `moonraker_update` | Runs a command or G-code the manifest names. Commands come only from the signed manifest, never from user input |
| `helix_install` | Uploads a HelixScreen release and its `install.sh` and runs it with `--progress` (see `docs/devel/INSTALLER.md` in the main repository). The release is resolved and checked before the step commits; with `check: true` the step only does that, without the printer, for a recipe whose install comes after an earlier point of no return (the lint requires one there). A new install takes the beta until 1.1.0 is the stable release |
| `serial_flash` | Writes a K-Touch's ESP32 image over USB after checking the chip and flash size |

Any step that changes the printer needs a `commits: true` step at or before it, and no recipe may have more than one: it marks the point of no return, after which an interrupted run is offered for recovery instead of a clean restart. `manifest-lint` (`crates/core/src/lint.rs`) enforces this and the rest of the structure: https artifacts, a restore recipe per printer, every referenced recipe, artifact and picture existing, a probe (or `wait_for`) re-identifying the printer after the commit before a step acts on it, and install and update recipes ending on a `wait_for helix_service` (or the K-Touch flash), so a run counts as done only once HelixScreen runs. Run it on both files:

```sh
cargo build --release -p helix_installer_core --bin manifest-lint
target/release/manifest-lint manifest/manifest.json manifest/img
target/release/manifest-lint manifest/staging.json manifest/img --allow-unverified
target/release/manifest-lint manifest/manifest.json manifest/img --signing   # what the publish job runs
target/release/manifest-lint --digests manifest/manifest.json               # each recipe's behavior digest
```

On the published list every recipe a printer offers needs a verified run that recorded its `behavior_sha256`: the digest of its steps with the user-facing words (`text`, `message`, `help`, `reason`, `doc`, `image`, ...) left out. Rewording a step keeps the run; any change to what a recipe does makes it unrun again. Until the new hardware run is recorded, name the recipe in the printer's `awaiting_rerun`: CI's lint passes, and `--signing` refuses it, so the list is not published.

## Adding a printer

1. Record the printer. Run the app, click the version number in the app's header five times to turn on record mode, and press "Record this printer" on its card. It prints a JSON fixture with addresses and the serial masked. Save it in `crates/core/tests/fixtures/probes/` (what identification sees); add a facts file to `fixtures/fleet/` if the recipe tests need one.
2. Add the printer to `manifest/staging.json`: `identify` rules that match it and nothing else, a `fallback.doc` that points at a real guide page (below), `recipes` by starting state, and a `restore` recipe. Reuse an existing recipe where the steps are the same.
3. Write the recipe steps. Take every command, path and version from the vendor's own instructions or the printer, and keep `ssh_run` commands idempotent. Put a `require` before anything destructive, and `commits: true` on the first step that cannot be undone.
4. Add tests first: an identification test in `tests/identify.rs` for the new fixture, and a run in `tests/recipes.rs` (the fakes need the printer's command answers). Run `manifest-lint` on `staging.json`.
5. Write or update the user guide in the main repository (`docs/user/guide/`), and list the page in the website's sync script and sidebar. Every help link must exist, see below.
6. Run the recipe on the real printer from the published list's recipe, then add a `verified` run to `manifest/publish.json` with the recipe's digest from `manifest-lint --digests`, drop it from `awaiting_rerun`, and run `scripts/publish_manifest.py`. After any later change to staging, run it again and commit both files.

## Help links

Every `https://helixscreen.org/...` link in the manifests and source is a help page. The site serves its guides at `https://helixscreen.org/guide/<page>/`, and `/installation/` for the general install guide; the pages are built from `docs/user/` in the main repository, and a new page needs an entry in the website's sync script and sidebar.

`scripts/check_help_links.py` finds every link and checks it. `--scheme` is offline and runs in CI on every push: it fails on an address the site doesn't serve. The live check (HEAD, then GET; redirects followed; non-200 fails; a link with a `#fragment` also fetches the page and needs an element with that id) runs daily in `.github/workflows/help-links.yml` and before every release in `release.yml`. Pages waiting on a site deploy are listed in `scripts/pending-help-pages.txt`, which the daily run tolerates and a release does not, so the file must be empty by release. When the site promotes 1.1 to stable, `/1.1/...` links stop resolving: change them (and the pending file) in the same step as the redirect or the link rewrite. The manifest tree at `/installer/v1/` is not a help page and is skipped.

## Signing and publishing

The manifest is minisign-signed and the app checks the signature against the key compiled into it. Release bundles are code-signed per platform, and the in-app updater has its own key. Setup, secrets and the publish flow are in [docs/SIGNING.md](docs/SIGNING.md).

## Branches

`engine` receives only commits CI has already passed: push to a `wip/<task>` branch, wait for a green run on all three operating systems, then fast-forward `engine` to that commit. Nothing is pushed to `main` by hand: a push there signs and publishes the manifest.
