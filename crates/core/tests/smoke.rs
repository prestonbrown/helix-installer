// SPDX-License-Identifier: GPL-3.0-or-later

#[test]
fn core_crate_links() {
    assert_eq!(helix_installer_core::version(), env!("CARGO_PKG_VERSION"));
    assert!(!helix_installer_core::version().is_empty());
}
