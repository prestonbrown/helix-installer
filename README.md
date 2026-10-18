# HelixScreen Installer

A desktop app that installs [HelixScreen](https://github.com/prestonbrown/helixscreen) onto 3D printers for people who would rather not use a terminal. The installer logic lives in `crates/core` (pure Rust, no Tauri), the Tauri 2 shell in `crates/app`, and the Svelte 5 + Vite + TypeScript interface in `ui/`.

To build and run: `cd ui && npm ci && npm run tauri dev` (Linux needs the webkit2gtk 4.1 development package). Run the Rust tests with `cargo test --workspace`, and the front-end checks with `npm run check` and `npm test` in `ui/`.

The printer list is a signed manifest. Until one is published, point the app at one with `HELIX_INSTALLER_MANIFEST` (a URL or file path; its signature sits beside it as `<location>.minisig`) and `HELIX_INSTALLER_PUBKEY` (the minisign public key, or a path to the `.pub` file). Without a list that verifies, the app shows "Can't load the printer list".

To see the interface without the Rust side, `npm run dev:mock` in `ui/` serves it against a mock of every command; `?screen=info` (or `user_action`, `ask`, `login`, `drive`, `drive_empty`, `progress`, `done`, `refused`, `failed`, `recovery`, `resume`, `no_list`) picks what an install shows after the first printer's button. `node scripts/shots.mjs <dir>` captures every screen headless, light and dark, at 1280x800 and 1024x640.
