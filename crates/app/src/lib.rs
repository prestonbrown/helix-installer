// SPDX-License-Identifier: GPL-3.0-or-later

pub mod commands;
pub mod updater;

use commands::{ipc, AppConfig, AppCore, LanDiscovery};
use helix_installer_core::app_log::AppLog;
use helix_installer_core::keys::{manifest_location, MANIFEST_PUBKEY};
use helix_installer_core::manifest::Channel;
use helix_installer_core::transport::drives_sys::SysDrives;
use helix_installer_core::transport::fetch_http::HttpFetch;
use helix_installer_core::transport::moonraker_http::HttpMoonraker;
use helix_installer_core::transport::net_tcp::TcpNet;
use helix_installer_core::transport::serial_espflash::EspflashSerial;
use helix_installer_core::transport::ssh_russh::SshRussh;
use helix_installer_core::transport::{Env, SystemClock};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tauri::Manager;

/// Overrides for development: where to fetch the printer list (`<url or path>`, signature at
/// `<url or path>.minisig`) and the public key it is signed with (key text or a `.pub` path).
/// The key override is honoured only in debug builds.
const MANIFEST_ENV: &str = "HELIX_INSTALLER_MANIFEST";
const PUBKEY_ENV: &str = "HELIX_INSTALLER_PUBKEY";
/// `stable` or `beta`: the HelixScreen release channel every install reads, over the recipes'.
const CHANNEL_ENV: &str = "HELIX_INSTALLER_CHANNEL";

/// The channel `value` names; an unset or empty variable is no override, an invalid one is
/// reported and ignored.
fn channel_override(value: Option<String>) -> Option<Channel> {
    let value = value.filter(|v| !v.is_empty())?;
    let channel = Channel::parse(&value);
    if channel.is_none() {
        eprintln!("ignoring {CHANNEL_ENV}={value:?}: expected \"stable\" or \"beta\"");
    }
    channel
}

/// The key the printer list must be signed with. `override_` (key text or a `.pub` path) is
/// honoured only when `allow_override`: a release build trusts the compiled-in key alone.
fn pubkey(override_: Option<String>, allow_override: bool) -> String {
    match override_.filter(|_| allow_override) {
        Some(v) if Path::new(&v).is_file() => std::fs::read_to_string(&v).unwrap_or_default(),
        Some(v) => v,
        None => MANIFEST_PUBKEY.to_string(),
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
        serial: Arc::new(EspflashSerial),
    }
}

fn app_core(data_dir: PathBuf, log: Option<Arc<AppLog>>) -> AppCore {
    AppCore::new(AppConfig {
        env: real_env(&data_dir),
        data_dir,
        discovery: Arc::new(LanDiscovery),
        pubkey: pubkey(std::env::var(PUBKEY_ENV).ok(), cfg!(debug_assertions)),
        manifest_location: Some(manifest_location(std::env::var(MANIFEST_ENV).ok())),
        channel_override: channel_override(std::env::var(CHANNEL_ENV).ok()),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        log,
    })
}

/// This launch's log in the platform log directory; the app runs without one if it cannot
/// be opened.
fn open_log(dir: std::io::Result<PathBuf>) -> Option<Arc<AppLog>> {
    let opened = dir.and_then(|d| AppLog::open(&d, SystemTime::now()));
    match opened {
        Ok(log) => Some(Arc::new(log)),
        Err(e) => {
            eprintln!("no app log: {e}");
            None
        }
    }
}

pub fn run() {
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    if let Some(updater) = updater::plugin() {
        builder = builder.plugin(updater);
    }
    builder
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let log = open_log(app.path().app_log_dir().map_err(std::io::Error::other));
            let core = app_core(data_dir, log);
            core.log(
                &format!(
                    "HelixScreen Installer {} started ({} {})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
                &[],
            );
            app.manage(core);
            app.manage(updater::PendingUpdate::default());
            tauri::async_runtime::spawn(updater::check(app.handle().clone()));
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
            ipc::dismiss_run,
            ipc::export_bundle,
            ipc::record_printer,
            ipc::install_update,
            ipc::pending_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running HelixScreen Installer");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_variable_overrides_only_when_valid() {
        assert_eq!(channel_override(Some("beta".into())), Some(Channel::Beta));
        assert_eq!(
            channel_override(Some("stable".into())),
            Some(Channel::Stable)
        );
        assert_eq!(channel_override(Some("nightly".into())), None);
        assert_eq!(channel_override(Some(String::new())), None);
        assert_eq!(channel_override(None), None);
    }

    #[test]
    fn only_a_development_build_takes_the_key_from_the_environment() {
        assert_eq!(pubkey(Some("RWdev".into()), true), "RWdev");
        assert_eq!(pubkey(Some("RWdev".into()), false), MANIFEST_PUBKEY);
        assert_eq!(pubkey(None, true), MANIFEST_PUBKEY);
    }
}
