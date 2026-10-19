// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::manifest::Login;
use helix_installer_core::transport::fake::FakeSerial;
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::*;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

fn login(user: &str, pw: &str) -> Login {
    Login::new(user, pw)
}

fn host() -> Host {
    Host::new("192.168.1.50")
}

#[test]
fn host_defaults() {
    let h = Host::new("printer.local");
    assert_eq!((h.moonraker_port, h.ssh_port), (7125, 22));
}

#[tokio::test]
async fn moonraker_replays_canned_json_and_records_calls() {
    let m = FakeMoonraker::new();
    m.set_printer_info("192.168.1.50", json!({"state": "ready"}));
    m.set_system_info("192.168.1.50", json!({"cpu": "mips"}));
    m.set_update_status("192.168.1.50", json!({"helixscreen": {}}));
    assert_eq!(m.printer_info(&host()).await.unwrap()["state"], "ready");
    assert_eq!(m.system_info(&host()).await.unwrap()["cpu"], "mips");
    assert!(m.update_status(&host()).await.unwrap()["helixscreen"].is_object());
    m.gcode(&host(), "RESTART").await.unwrap();
    m.update_upgrade(&host(), "helixscreen").await.unwrap();
    assert_eq!(
        m.gcodes(),
        vec![("192.168.1.50".to_string(), "RESTART".to_string())]
    );
    assert_eq!(m.upgrades(), vec!["helixscreen".to_string()]);
    assert_eq!(
        m.printer_info(&Host::new("10.0.0.9")).await.unwrap_err(),
        TransportError::Unreachable
    );
}

#[tokio::test]
async fn ssh_accepts_only_registered_logins() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "creality");
    assert!(ssh
        .connect(&host(), &login("root", "creality"))
        .await
        .is_ok());
    assert_eq!(
        ssh.connect(&host(), &login("root", "nope")).await.err(),
        Some(TransportError::Auth)
    );
    assert_eq!(
        ssh.connect(&Host::new("10.0.0.9"), &login("root", "creality"))
            .await
            .err(),
        Some(TransportError::Unreachable)
    );
}

#[tokio::test]
async fn ssh_exec_uses_table_then_fallback_and_keeps_stdin_apart() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    ssh.on_command(
        "uname -m",
        ExecOutput {
            stdout: "mips\n".into(),
            stderr: String::new(),
            exit: 0,
        },
    );
    ssh.set_fallback(|cmd| ExecOutput {
        stdout: format!("fallback:{cmd}"),
        stderr: String::new(),
        exit: 3,
    });
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let t = Duration::from_secs(5);
    assert_eq!(s.exec("uname -m", None, t).await.unwrap().stdout, "mips\n");
    let o = s.exec("sudo -S id", Some(b"s3cret\n"), t).await.unwrap();
    assert_eq!((o.stdout.as_str(), o.exit), ("fallback:sudo -S id", 3));
    assert_eq!(ssh.commands(), vec!["uname -m", "sudo -S id"]);
    assert_eq!(ssh.stdins(), vec![b"s3cret\n".to_vec()]);
    assert!(ssh.commands().iter().all(|c| !c.contains("s3cret")));
}

#[tokio::test]
async fn ssh_unscripted_command_fails_with_127() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let o = s.exec("nope", None, Duration::from_secs(1)).await.unwrap();
    assert_eq!(o.exit, 127);
}

#[tokio::test]
async fn ssh_upload_records_remote_path_and_bytes() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("a.bin");
    std::fs::write(&local, b"payload").unwrap();
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    s.upload(&local, "/tmp/a.bin").await.unwrap();
    assert_eq!(
        ssh.uploads(),
        vec![("/tmp/a.bin".to_string(), b"payload".to_vec())]
    );
}

#[tokio::test]
async fn ssh_job_log_reveals_one_chunk_per_read() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    ssh.script_job(vec![("one\n", None), ("two\n", None), ("done\n", Some(0))]);
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let job = s.spawn_detached("install.sh", Some("pw")).await.unwrap();
    assert_eq!(ssh.commands(), vec!["install.sh"]);
    assert_eq!(ssh.stdins(), vec![b"pw".to_vec()]);
    let c1 = s.read_job(&job, 0).await.unwrap();
    assert_eq!(
        (c1.text.as_str(), c1.next_offset, c1.exit),
        ("one\n", 4, None)
    );
    let c2 = s.read_job(&job, c1.next_offset).await.unwrap();
    assert_eq!((c2.text.as_str(), c2.next_offset), ("two\n", 8));
    let c3 = s.read_job(&job, c2.next_offset).await.unwrap();
    assert_eq!((c3.text.as_str(), c3.exit), ("done\n", Some(0)));
}

#[tokio::test]
async fn ssh_drop_after_kills_session_but_reconnect_resumes_job() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    ssh.script_job(vec![("a", None), ("b", None), ("c", Some(0))]);
    ssh.drop_after(3);
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let job = s.spawn_detached("x", None).await.unwrap(); // call 1
    let c = s.read_job(&job, 0).await.unwrap(); // call 2
    assert_eq!(c.text, "a");
    assert_eq!(
        s.read_job(&job, c.next_offset).await.unwrap_err(),
        TransportError::Unreachable
    ); // call 3
    assert_eq!(
        s.exec("id", None, Duration::from_secs(1))
            .await
            .unwrap_err(),
        TransportError::Unreachable
    );
    let mut s2 = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let c = s2.read_job(&job, c.next_offset).await.unwrap();
    assert_eq!(c.text, "b");
    let c = s2.read_job(&job, c.next_offset).await.unwrap();
    assert_eq!((c.text.as_str(), c.exit), ("c", Some(0)));
}

fn usb(id: &str) -> Drive {
    Drive {
        id: id.into(),
        label: "HELIX".into(),
        mount: PathBuf::from("/media/helix"),
        fs: "vfat".into(),
        size_bytes: 8_000_000_000,
        free_bytes: 7_000_000_000,
        removable: true,
        system: false,
    }
}

#[tokio::test]
async fn drives_copy_reads_back_and_eject_is_recorded() {
    let drives = FakeDrives::new();
    drives.add(usb("d1"));
    let mut sys = usb("c");
    sys.system = true;
    drives.add(sys);
    let listed = drives.list_drives().unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[1].system);

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("fw.bin");
    std::fs::write(&src, b"firmware").unwrap();
    drives
        .copy_to(&listed[0], &src, "update.bin")
        .await
        .unwrap();
    assert_eq!(
        drives.file("d1", "update.bin").as_deref(),
        Some(&b"firmware"[..])
    );
    drives.eject(&listed[0]).await.unwrap();
    assert_eq!(drives.ejected(), vec!["d1".to_string()]);
    assert_eq!(
        drives.copy_to(&usb("zz"), &src, "x").await.unwrap_err(),
        TransportError::Io("unknown drive zz".into())
    );
}

#[tokio::test]
async fn fetch_serves_map_reports_progress_and_404s() {
    let f = FakeFetch::new();
    f.add("https://x/a.bin", b"12345".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.bin");
    let seen = Mutex::new(Vec::new());
    f.get("https://x/a.bin", &dest, &|d, t| {
        seen.lock().unwrap().push((d, t))
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"12345");
    assert_eq!(seen.into_inner().unwrap().last(), Some(&(5, Some(5))));
    assert_eq!(f.get_text("https://x/a.bin").await.unwrap(), "12345");
    assert_eq!(
        f.get_text("https://x/missing").await.unwrap_err(),
        TransportError::Http(404)
    );
    assert_eq!(f.requests().len(), 3);
}

#[tokio::test]
async fn clock_advances_on_sleep_without_waiting() {
    let c = FakeClock::new();
    let t0 = c.now();
    c.sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        c.now().duration_since(t0).unwrap(),
        Duration::from_secs(3600)
    );
}

#[test]
fn env_bundles_fakes() {
    use std::sync::Arc;
    let _env = Env {
        moonraker: Arc::new(FakeMoonraker::new()),
        ssh: Arc::new(FakeSsh::new()),
        drives: Arc::new(FakeDrives::new()),
        fetch: Arc::new(FakeFetch::new()),
        clock: Arc::new(FakeClock::new()),
        net: Arc::new(FakeNet::new()),
        serial: Arc::new(FakeSerial::new()),
    };
}

#[tokio::test]
async fn ssh_calls_pair_each_command_with_its_stdin() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    ssh.script_job(vec![("", Some(0))]);
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let t = Duration::from_secs(1);
    s.exec("a", None, t).await.unwrap();
    s.exec("b", Some(b"x"), t).await.unwrap();
    s.spawn_detached("c", Some("pw")).await.unwrap();
    assert_eq!(
        ssh.calls(),
        vec![
            ("a".to_string(), None),
            ("b".to_string(), Some(b"x".to_vec())),
            ("c".to_string(), Some(b"pw".to_vec())),
        ]
    );
}

#[tokio::test]
async fn ssh_read_job_rejects_bad_offsets() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    ssh.script_job(vec![("h\u{e9}llo", Some(0))]);
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    let job = s.spawn_detached("x", None).await.unwrap();
    assert!(matches!(
        s.read_job(&job, 99).await,
        Err(TransportError::Protocol(_))
    ));
    assert!(matches!(
        s.read_job(&job, 2).await,
        Err(TransportError::Protocol(_))
    ));
}

#[tokio::test]
async fn ssh_spawn_without_scripted_job_is_a_protocol_error() {
    let ssh = FakeSsh::new();
    ssh.accept("192.168.1.50", "root", "pw");
    let mut s = ssh.connect(&host(), &login("root", "pw")).await.unwrap();
    assert_eq!(
        s.spawn_detached("run.sh", None).await.unwrap_err(),
        TransportError::Protocol("unscripted job: run.sh".into())
    );
}
