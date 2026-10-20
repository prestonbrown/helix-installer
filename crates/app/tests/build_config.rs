// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use std::path::{Path, PathBuf};

fn app_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The Tauri CLI runs `beforeDevCommand` and `beforeBuildCommand` in the frontend directory it
/// resolves, which from the repository root is `ui/`. Returns the directory npm would read its
/// `package.json` from when `command` runs there.
fn npm_project_dir(command: &str, frontend: &Path) -> PathBuf {
    let words: Vec<&str> = command.split_whitespace().collect();
    match words.iter().position(|w| *w == "--prefix") {
        Some(i) => frontend.join(words.get(i + 1).copied().unwrap_or("")),
        None => frontend.to_path_buf(),
    }
}

#[test]
fn the_before_commands_run_the_ui_project_from_the_frontend_directory() {
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(app_dir().join("tauri.conf.json")).unwrap())
            .unwrap();
    let frontend = app_dir().join("../../ui").canonicalize().unwrap();
    for key in ["beforeDevCommand", "beforeBuildCommand"] {
        let command = config["build"][key].as_str().unwrap();
        let dir = npm_project_dir(command, &frontend);
        assert!(
            dir.join("package.json").is_file(),
            "{key} `{command}` reads {}, which has no package.json",
            dir.display()
        );
        assert_eq!(dir.canonicalize().unwrap(), frontend, "{key} `{command}`");
    }
}
