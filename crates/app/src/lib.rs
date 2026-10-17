// SPDX-License-Identifier: GPL-3.0-or-later

#[tauri::command]
fn core_version() -> &'static str {
    helix_installer_core::version()
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![core_version])
        .run(tauri::generate_context!())
        .expect("error while running HelixScreen Installer");
}
