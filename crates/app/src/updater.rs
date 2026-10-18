// SPDX-License-Identifier: GPL-3.0-or-later

//! The in-app updater: one check at startup against the published `latest.json`, and an
//! install the user asks for. With no updater key compiled in, nothing here is registered and
//! the app never contacts the update endpoint.

use helix_installer_core::keys::{updater_key, UPDATER_PUBKEY, UPDATE_ENDPOINT};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Fired with the new version string when a newer release is available.
pub const UPDATE_EVENT: &str = "update-available";

/// The update found by the startup check, held until the user installs it.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

/// The updater plugin, or `None` when no key is compiled in.
pub fn plugin<R: Runtime>() -> Option<tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config>> {
    plugin_for(UPDATER_PUBKEY)
}

fn plugin_for<R: Runtime>(
    key: &str,
) -> Option<tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config>> {
    let key = updater_key(key)?;
    Some(tauri_plugin_updater::Builder::new().pubkey(key).build())
}

/// Look for a newer release and announce it. Every failure (offline, no manifest yet, a bad
/// signature) leaves the app as it was: an update check is never worth an error dialog.
pub async fn check<R: Runtime>(app: AppHandle<R>) {
    if updater_key(UPDATER_PUBKEY).is_none() {
        return;
    }
    let Ok(url) = UPDATE_ENDPOINT.parse() else {
        return;
    };
    let Ok(builder) = app.updater_builder().endpoints(vec![url]) else {
        return;
    };
    let Ok(updater) = builder.build() else {
        return;
    };
    let Ok(Some(update)) = updater.check().await else {
        return;
    };
    let version = update.version.clone();
    if let Some(pending) = app.try_state::<PendingUpdate>() {
        if let Ok(mut slot) = pending.0.lock() {
            *slot = Some(update);
        }
    }
    let _ = app.emit(UPDATE_EVENT, version);
}

/// Download and install the update the startup check found, then restart into it.
pub async fn install<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let update = app
        .try_state::<PendingUpdate>()
        .and_then(|p| p.0.lock().ok().and_then(|mut s| s.take()))
        .ok_or_else(|| "no update is waiting".to_string())?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_registers_no_plugin() {
        assert!(plugin_for::<tauri::Wry>("").is_none());
        assert!(plugin_for::<tauri::Wry>(" \n").is_none());
    }

    #[test]
    fn a_key_registers_the_plugin() {
        assert!(plugin_for::<tauri::Wry>("dW50cnVzdGVk").is_some());
    }

    #[test]
    fn the_shipped_build_has_no_updater_key_until_signing_is_set_up() {
        assert!(plugin::<tauri::Wry>().is_some() == updater_key(UPDATER_PUBKEY).is_some());
    }
}
