// SPDX-License-Identifier: GPL-3.0-or-later

#[test]
fn core_crate_links() {
    assert_eq!(helix_installer_core::version(), env!("CARGO_PKG_VERSION"));
    assert!(!helix_installer_core::version().is_empty());
}

#[tokio::test]
async fn system_clock_reads_the_wall_clock() {
    use helix_installer_core::transport::{Clock, SystemClock};
    use std::time::{Duration, SystemTime};
    let before = SystemTime::now();
    let now = SystemClock.now();
    assert!(now >= before && now <= SystemTime::now());
    SystemClock.sleep(Duration::from_millis(1)).await;
    assert!(SystemClock.now() > before);
}
