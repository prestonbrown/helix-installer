// SPDX-License-Identifier: GPL-3.0-or-later

//! `Moonraker` over HTTP with reqwest.

use super::{Host, Moonraker, Result, TransportError};
use async_trait::async_trait;
use reqwest::{Client, Method};
use serde_json::Value;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const GCODE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HttpMoonraker {
    client: Client,
    request_timeout: Duration,
    gcode_timeout: Duration,
}

impl Default for HttpMoonraker {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a reqwest failure to the transport's vocabulary.
pub(crate) fn map_err(e: reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout
    } else if e.is_connect() {
        TransportError::Unreachable
    } else {
        TransportError::Protocol(e.to_string())
    }
}

impl HttpMoonraker {
    pub fn new() -> Self {
        Self::with_timeouts(CONNECT_TIMEOUT, REQUEST_TIMEOUT, GCODE_TIMEOUT)
    }

    pub fn with_timeouts(connect: Duration, request: Duration, gcode: Duration) -> Self {
        // Building a client with these options only fails when the TLS backend cannot
        // initialise; the default client then is the best remaining option.
        let client = Client::builder()
            .connect_timeout(connect)
            .build()
            .unwrap_or_default();
        HttpMoonraker {
            client,
            request_timeout: request,
            gcode_timeout: gcode,
        }
    }

    async fn call(
        &self,
        method: Method,
        host: &Host,
        path: &str,
        query: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Value> {
        let url = format!("http://{}:{}{path}", host.addr, host.moonraker_port);
        let resp = self
            .client
            .request(method, url)
            .query(query)
            .timeout(timeout)
            .send()
            .await
            .map_err(map_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(TransportError::Http(status.as_u16()));
        }
        let mut body: Value = resp.json().await.map_err(map_err)?;
        Ok(body.get_mut("result").map(Value::take).unwrap_or(body))
    }

    async fn get(&self, host: &Host, path: &str) -> Result<Value> {
        self.call(Method::GET, host, path, &[], self.request_timeout)
            .await
    }
}

#[async_trait]
impl Moonraker for HttpMoonraker {
    async fn printer_info(&self, host: &Host) -> Result<Value> {
        self.get(host, "/printer/info").await
    }
    async fn system_info(&self, host: &Host) -> Result<Value> {
        self.get(host, "/machine/system_info").await
    }
    async fn update_status(&self, host: &Host) -> Result<Value> {
        self.get(host, "/machine/update/status").await
    }
    async fn objects_list(&self, host: &Host) -> Result<Value> {
        self.get(host, "/printer/objects/list").await
    }
    async fn gcode(&self, host: &Host, script: &str) -> Result<()> {
        self.call(
            Method::POST,
            host,
            "/printer/gcode/script",
            &[("script", script)],
            self.gcode_timeout,
        )
        .await
        .map(drop)
    }
    async fn update_upgrade(&self, host: &Host, name: &str) -> Result<()> {
        self.call(
            Method::POST,
            host,
            "/machine/update/upgrade",
            &[("name", name)],
            self.request_timeout,
        )
        .await
        .map(drop)
    }
}
