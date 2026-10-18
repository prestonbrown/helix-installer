// SPDX-License-Identifier: GPL-3.0-or-later

pub mod commands;

use commands::{ipc, AppConfig, AppCore, LanDiscovery};
use helix_installer_core::transport::drives_sys::SysDrives;
use helix_installer_core::transport::fetch_http::HttpFetch;
use helix_installer_core::transport::moonraker_http::HttpMoonraker;
use helix_installer_core::transport::net_tcp::TcpNet;
use helix_installer_core::transport::ssh_russh::SshRussh;
use helix_installer_core::transport::{Env, SystemClock};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::Manager;

/// The key the published printer list is signed with. Empty until the list is published, so
/// until then only a key given in `HELIX_INSTALLER_PUBKEY` verifies anything.
const MANIFEST_PUBKEY: &str = "";

/// Overrides for development: where to fetch the printer list (`<url or path>`, signature at
/// `<url or path>.minisig`) and the public key it is signed with (key text or a `.pub` path).
const MANIFEST_ENV: &str = "HELIX_INSTALLER_MANIFEST";
const PUBKEY_ENV: &str = "HELIX_INSTALLER_PUBKEY";

fn pubkey() -> String {
    match std::env::var(PUBKEY_ENV) {
        Ok(v) if Path::new(&v).is_file() => std::fs::read_to_string(&v).unwrap_or_default(),
        Ok(v) => v,
        Err(_) => MANIFEST_PUBKEY.to_string(),
    }
}

fn real_env(data_dir: &Path) -> Env {
    Env {
        moonraker: Arc::new(HttpMoonraker::new()),
        ssh: Arc::new(SshRussh::new(data_dir.join("known_hosts.json"))),
        drives: Arc::new(SysDrives::new()),
        fetch: Arc::new(HttpFetch::new()),
        clock: Arc::new(SystemClock),
        net: Arc::new(TcpNet),
    }
}

fn app_core(data_dir: PathBuf) -> AppCore {
    AppCore::new(AppConfig {
        env: real_env(&data_dir),
        data_dir,
        discovery: Arc::new(LanDiscovery),
        pubkey: pubkey(),
        manifest_location: std::env::var(MANIFEST_ENV).ok().filter(|v| !v.is_empty()),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            app.manage(app_core(data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::app_info,
            ipc::printers,
            ipc::discover,
            ipc::add_manual_host,
            ipc::start_run,
            ipc::advance,
            ipc::unfinished_runs,
            ipc::export_bundle,
            ipc::record_printer,
        ])
        .run(tauri::generate_context!())
        .expect("error while running HelixScreen Installer");
}
