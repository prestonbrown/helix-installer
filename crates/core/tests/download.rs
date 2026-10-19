// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::manifest::Artifact;
use helix_installer_core::steps::download::{fetch_artifact, DownloadError};
use helix_installer_core::transport::fake::FakeSerial;
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
        serial: Arc::new(FakeSerial::new()),
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

/// Holds every download open until `writers` of them have their part files on disk, so the
/// writers overlap the way two runs fetching one artifact do.
struct OverlappingFetch {
    body: Vec<u8>,
    barrier: tokio::sync::Barrier,
    dests: Mutex<Vec<std::path::PathBuf>>,
}

#[async_trait::async_trait]
impl helix_installer_core::transport::Fetch for OverlappingFetch {
    async fn get(
        &self,
        _url: &str,
        dest: &std::path::Path,
        _progress: &(dyn Fn(u64, Option<u64>) + Sync),
    ) -> Result<(), TransportError> {
        self.dests.lock().unwrap().push(dest.to_path_buf());
        std::fs::write(dest, &self.body).map_err(|e| TransportError::Io(e.to_string()))?;
        self.barrier.wait().await;
        Ok(())
    }
    async fn get_text(&self, _url: &str) -> Result<String, TransportError> {
        Err(TransportError::Http(404))
    }
}

#[tokio::test]
async fn concurrent_writers_of_one_artifact_never_share_a_part_file() {
    let body = b"one artifact, two runs".to_vec();
    let fetch = Arc::new(OverlappingFetch {
        body: body.clone(),
        barrier: tokio::sync::Barrier::new(2),
        dests: Mutex::new(Vec::new()),
    });
    let mut env = env_with(Arc::new(FakeFetch::new()));
    env.fetch = fetch.clone();
    let dir = tempfile::tempdir().unwrap();
    let a = artifact(hex(&body));
    let (one, two) = tokio::join!(
        fetch_artifact(&env, &a, dir.path(), &|_, _| {}),
        fetch_artifact(&env, &a, dir.path(), &|_, _| {}),
    );
    assert_eq!(one.unwrap(), dir.path().join(&a.sha256));
    assert_eq!(two.unwrap(), dir.path().join(&a.sha256));
    let dests = fetch.dests.lock().unwrap().clone();
    assert_eq!(dests.len(), 2);
    assert_ne!(dests[0], dests[1], "both writers used {:?}", dests[0]);
    assert!(dests.iter().all(|d| d.parent() == Some(dir.path())));
    assert_eq!(names(dir.path()), vec![a.sha256.clone()]);
    assert_eq!(std::fs::read(dir.path().join(&a.sha256)).unwrap(), body);
}
