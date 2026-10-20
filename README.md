# HelixScreen Installer

A desktop app that installs [HelixScreen](https://github.com/prestonbrown/helixscreen) onto 3D printers for people who would rather not use a terminal. The installer logic lives in `crates/core` (pure Rust, no Tauri), the Tauri 2 shell in `crates/app`, and the Svelte 5 + Vite + TypeScript interface in `ui/`.

To build and run: `npm ci --prefix ui`, then from the repository root `ui/node_modules/.bin/tauri dev` (or `cargo tauri dev` with `cargo install tauri-cli`); the CLI finds `crates/app` and runs the UI's own scripts in `ui/` (Linux needs the webkit2gtk 4.1 development package). Run the Rust tests with `cargo test --workspace`, and the front-end checks with `npm run check` and `npm test` in `ui/`.

The printer list is a signed manifest. Until one is published, point the app at one with `HELIX_INSTALLER_MANIFEST` (a URL or file path; its signature sits beside it as `<location>.minisig`) and `HELIX_INSTALLER_PUBKEY` (the minisign public key, or a path to the `.pub` file; honoured only by debug builds such as `cargo tauri dev`). Without a list that verifies, the app shows "Can't load the printer list".

To see the interface without the Rust side, `npm run dev:mock` in `ui/` serves it against a mock of every command; `?screen=info` (or `user_action`, `ask`, `login`, `drive`, `drive_empty`, `progress`, `done`, `refused`, `failed`, `recovery`, `resume`, `no_list`) picks what an install shows after the first printer's button. `node scripts/shots.mjs <dir>` captures every screen headless, light and dark, at 1280x800 and 1024x640.

`e2e/run-xvfb.sh` drives a debug build (`cargo tauri build --debug --no-bundle`) through one update run on a real printer, on a private Xvfb display, over WebDriver: it needs `cargo install tauri-driver` and `WebKitWebDriver` (the `webkit2gtk-driver` package). The variables it reads are listed at the top of `e2e/run-update.mjs`; the password is read from the variable `E2E_PW_VAR` names and never printed. It stops at any screen it does not know how to answer, so it never agrees to a change on the printer's behalf. It is not part of CI.
