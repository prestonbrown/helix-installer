# HelixScreen Installer

A desktop app that installs [HelixScreen](https://github.com/prestonbrown/helixscreen) onto 3D printers for people who would rather not use a terminal. The installer logic lives in `crates/core` (pure Rust, no Tauri), the Tauri 2 shell in `crates/app`, and the Svelte 5 + Vite + TypeScript interface in `ui/`.

To build and run: `cd ui && npm ci && npm run tauri dev` (Linux needs the webkit2gtk 4.1 development package). Run the Rust tests with `cargo test --workspace`, and the front-end checks with `npm run check` in `ui/`.
