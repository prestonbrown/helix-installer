// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::manifest::Artifact;
use helix_installer_core::steps::download::{fetch_artifact, DownloadError};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Env, TransportError};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

const URL: &str = "https://example.test/fw.bin";

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn env_with(fetch: Arc<FakeFetch>) -> Env {
    Env {
        moonraker: Arc::new(FakeMoonraker::new()),
        ssh: Arc::new(FakeSsh::new()),
        drives: Arc::new(FakeDrives::new()),
        fetch,
        clock: Arc::new(FakeClock::new()),
        net: Arc::new(FakeNet::new()),
    }
}

fn artifact(sha256: String) -> Artifact {
    Artifact {
        url: URL.into(),
        sha256,
        filename: Some("fw.bin".into()),
    }
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn downloads_verifies_and_caches_by_hash() {
    let body = b"firmware bytes".to_vec();
    let fetch = Arc::new(FakeFetch::new());
    fetch.add(URL, body.clone());
    let dir = tempfile::tempdir().unwrap();
    let a = artifact(hex(&body));
    let seen = Mutex::new(Vec::new());
    let path = fetch_artifact(&env_with(fetch), &a, dir.path(), &|d, t| {
        seen.lock().unwrap().push((d, t))
    })
    .await
    .unwrap();
    assert_eq!(path, dir.path().join(&a.sha256));
    assert_eq!(std::fs::read(&path).unwrap(), body);
    assert_eq!(names(dir.path()), vec![a.sha256.clone()]);
    let n = body.len() as u64;
    assert_eq!(seen.lock().unwrap().last(), Some(&(n, Some(n))));
}

#[tokio::test]
async fn wrong_bytes_give_hash_mismatch_and_leave_no_file() {
    let fetch = Arc::new(FakeFetch::new());
    fetch.add(URL, b"tampered".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let expected = hex(b"the real firmware");
    let err = fetch_artifact(
        &env_with(fetch),
        &artifact(expected.clone()),
        dir.path(),
        &|_, _| {},
    )
    .await
    .unwrap_err();
    match err {
        DownloadError::HashMismatch { expected: e, got } => {
            assert_eq!(e, expected);
            assert_eq!(got, hex(b"tampered"));
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    assert!(names(dir.path()).is_empty());
}

#[tokio::test]
async fn correct_cached_file_is_reused_without_a_fetch() {
    let body = b"cached firmware".to_vec();
    let fetch = Arc::new(FakeFetch::new());
    let dir = tempfile::tempdir().unwrap();
    let a = artifact(hex(&body));
    std::fs::write(dir.path().join(&a.sha256), &body).unwrap();
    let path = fetch_artifact(&env_with(fetch.clone()), &a, dir.path(), &|_, _| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), body);
    assert!(fetch.requests().is_empty());
}

#[tokio::test]
async fn corrupted_cached_file_is_refetched() {
    let body = b"good firmware".to_vec();
    let fetch = Arc::new(FakeFetch::new());
    fetch.add(URL, body.clone());
    let dir = tempfile::tempdir().unwrap();
    let a = artifact(hex(&body));
    std::fs::write(dir.path().join(&a.sha256), b"bit rot").unwrap();
    let path = fetch_artifact(&env_with(fetch.clone()), &a, dir.path(), &|_, _| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), body);
    assert_eq!(fetch.requests(), vec![URL.to_string()]);
}

#[tokio::test]
async fn corrupted_cache_and_bad_server_bytes_end_with_nothing_cached() {
    let fetch = Arc::new(FakeFetch::new());
    fetch.add(URL, b"still wrong".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let a = artifact(hex(b"expected"));
    std::fs::write(dir.path().join(&a.sha256), b"bit rot").unwrap();
    let err = fetch_artifact(&env_with(fetch.clone()), &a, dir.path(), &|_, _| {})
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::HashMismatch { .. }));
    assert!(names(dir.path()).is_empty());
    assert_eq!(fetch.requests().len(), 1);
}

#[tokio::test]
async fn network_failure_is_reported_and_leaves_no_partial() {
    let fetch = Arc::new(FakeFetch::new());
    let dir = tempfile::tempdir().unwrap();
    let err = fetch_artifact(
        &env_with(fetch),
        &artifact(hex(b"x")),
        dir.path(),
        &|_, _| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        DownloadError::Network(TransportError::Http(404))
    ));
    assert!(names(dir.path()).is_empty());
}

#[tokio::test]
async fn creates_a_missing_cache_dir() {
    let body = b"x".to_vec();
    let fetch = Arc::new(FakeFetch::new());
    fetch.add(URL, body.clone());
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("a").join("cache");
    let path = fetch_artifact(&env_with(fetch), &artifact(hex(&body)), &cache, &|_, _| {})
        .await
        .unwrap();
    assert!(path.exists());
}

#[tokio::test]
async fn rejects_an_expected_hash_that_is_not_lowercase_hex() {
    let fetch = Arc::new(FakeFetch::new());
    fetch.add(URL, b"x".to_vec());
    let dir = tempfile::tempdir().unwrap();
    for bad in ["../../etc/passwd", &hex(b"x").to_uppercase(), ""] {
        let err = fetch_artifact(
            &env_with(fetch.clone()),
            &artifact(bad.into()),
            dir.path(),
            &|_, _| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DownloadError::Io(_)), "{bad}");
    }
    assert!(fetch.requests().is_empty());
}
