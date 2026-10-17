// SPDX-License-Identifier: GPL-3.0-or-later

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use helix_installer_core::transport::fetch_http::HttpFetch;
use helix_installer_core::transport::moonraker_http::HttpMoonraker;
use helix_installer_core::transport::{Fetch, Host, Moonraker, TransportError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<String>>>;

async fn serve(app: Router) -> Host {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Host {
        addr: "127.0.0.1".into(),
        moonraker_port: port,
        ssh_port: 22,
    }
}

async fn script(State(seen): State<Seen>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    seen.lock().unwrap().push(format!("script={}", q["script"]));
    Json(json!({"result": "ok"}))
}

async fn upgrade(
    State(seen): State<Seen>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    seen.lock().unwrap().push(format!("name={}", q["name"]));
    Json(json!({"result": "ok"}))
}

fn moonraker_app(seen: Seen) -> Router {
    Router::new()
        .route(
            "/printer/info",
            get(|| async { Json(json!({"result": {"hostname": "voron", "state": "ready"}})) }),
        )
        .route(
            "/machine/system_info",
            get(|| async { Json(json!({"result": {"system_info": {}}})) }),
        )
        .route(
            "/printer/objects/list",
            get(|| async { Json(json!({"result": {"objects": ["extruder"]}})) }),
        )
        .route(
            "/machine/update/status",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route("/printer/gcode/script", post(script))
        .route("/machine/update/upgrade", post(upgrade))
        .with_state(seen)
}

#[tokio::test]
async fn reads_unwrap_the_result_envelope() {
    let host = serve(moonraker_app(Seen::default())).await;
    let mr = HttpMoonraker::new();
    assert_eq!(mr.printer_info(&host).await.unwrap()["hostname"], "voron");
    assert!(mr
        .system_info(&host)
        .await
        .unwrap()
        .get("system_info")
        .is_some());
    assert_eq!(
        mr.objects_list(&host).await.unwrap()["objects"],
        json!(["extruder"])
    );
}

#[tokio::test]
async fn a_non_2xx_answer_is_an_http_error() {
    let host = serve(moonraker_app(Seen::default())).await;
    assert_eq!(
        HttpMoonraker::new().update_status(&host).await,
        Err(TransportError::Http(404))
    );
}

#[tokio::test]
async fn gcode_and_upgrade_post_their_query_parameters() {
    let seen = Seen::default();
    let host = serve(moonraker_app(seen.clone())).await;
    let mr = HttpMoonraker::new();
    mr.gcode(&host, "G28 X").await.unwrap();
    mr.update_upgrade(&host, "helixscreen").await.unwrap();
    assert_eq!(*seen.lock().unwrap(), ["script=G28 X", "name=helixscreen"]);
}

#[tokio::test]
async fn a_refused_connection_is_unreachable() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let host = Host {
        addr: "127.0.0.1".into(),
        moonraker_port: port,
        ssh_port: 22,
    };
    assert_eq!(
        HttpMoonraker::new().printer_info(&host).await,
        Err(TransportError::Unreachable)
    );
}

#[tokio::test]
async fn a_stalled_server_times_out() {
    let app = Router::new().route(
        "/printer/info",
        get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            "late"
        }),
    );
    let host = serve(app).await;
    let mr = HttpMoonraker::with_timeouts(
        std::time::Duration::from_millis(500),
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(200),
    );
    assert_eq!(mr.printer_info(&host).await, Err(TransportError::Timeout));
}

fn fetch_app() -> Router {
    Router::new()
        .route("/big.bin", get(|| async { vec![7u8; 300_000] }))
        .route("/text", get(|| async { "hello" }))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
}

#[tokio::test]
async fn fetch_streams_to_disk_and_reports_progress() {
    let host = serve(fetch_app()).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("big.bin");
    let ticks: Mutex<Vec<(u64, Option<u64>)>> = Mutex::new(Vec::new());
    HttpFetch::new()
        .get(
            &format!("http://127.0.0.1:{}/big.bin", host.moonraker_port),
            &dest,
            &|n, total| ticks.lock().unwrap().push((n, total)),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), vec![7u8; 300_000]);
    let ticks = ticks.lock().unwrap();
    assert_eq!(ticks.last(), Some(&(300_000, Some(300_000))));
}

#[tokio::test]
async fn fetch_truncates_an_existing_destination() {
    let host = serve(fetch_app()).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("t");
    std::fs::write(&dest, vec![1u8; 1_000_000]).unwrap();
    HttpFetch::new()
        .get(
            &format!("http://127.0.0.1:{}/text", host.moonraker_port),
            &dest,
            &|_, _| {},
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
}

#[tokio::test]
async fn fetch_maps_http_errors_and_reads_text() {
    let host = serve(fetch_app()).await;
    let base = format!("http://127.0.0.1:{}", host.moonraker_port);
    let f = HttpFetch::new();
    assert_eq!(f.get_text(&format!("{base}/text")).await.unwrap(), "hello");
    assert_eq!(
        f.get_text(&format!("{base}/missing")).await,
        Err(TransportError::Http(404))
    );
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        f.get(
            &format!("{base}/missing"),
            &dir.path().join("x"),
            &|_, _| {}
        )
        .await,
        Err(TransportError::Http(404))
    );
}
