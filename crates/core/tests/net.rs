// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::transport::fake::FakeNet;
use helix_installer_core::transport::net_tcp::TcpNet;
use helix_installer_core::transport::{Host, Net, TransportError};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn local() -> Host {
    Host::new("127.0.0.1")
}

#[tokio::test]
async fn tcp_banner_returns_the_first_line() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(b"SSH-2.0-dropbear\r\nmore").await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    let got = TcpNet
        .tcp_banner(&local(), port, Duration::from_secs(2))
        .await;
    assert_eq!(got, Ok(Some("SSH-2.0-dropbear".to_string())));
}

#[tokio::test]
async fn a_silent_server_is_connected_without_a_banner() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (_s, _) = l.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let got = TcpNet
        .tcp_banner(&local(), port, Duration::from_millis(200))
        .await;
    assert_eq!(got, Ok(None));
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let got = TcpNet
        .tcp_banner(&local(), port, Duration::from_secs(1))
        .await;
    assert_eq!(got, Err(TransportError::Unreachable));
}

#[tokio::test]
async fn fake_net_answers_only_what_is_scripted() {
    let n = FakeNet::new();
    n.answer("10.0.0.1", 22, Some("SSH-2.0-x"));
    n.answer("10.0.0.1", 80, None);
    let h = Host::new("10.0.0.1");
    let t = Duration::from_secs(1);
    assert_eq!(
        n.tcp_banner(&h, 22, t).await,
        Ok(Some("SSH-2.0-x".to_string()))
    );
    assert_eq!(n.tcp_banner(&h, 80, t).await, Ok(None));
    assert_eq!(
        n.tcp_banner(&h, 81, t).await,
        Err(TransportError::Unreachable)
    );
}
