// SPDX-License-Identifier: GPL-3.0-or-later

//! `Fetch` over HTTP(S) with reqwest, streaming downloads to disk.

use super::moonraker_http::map_err;
use super::{Fetch, Result, TransportError};
use async_trait::async_trait;
use reqwest::Client;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a download may stall between chunks.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpFetch {
    client: Client,
}

impl Default for HttpFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFetch {
    pub fn new() -> Self {
        // GitHub's API refuses requests that carry no User-Agent.
        let client = Client::builder()
            .user_agent(concat!("helix-installer/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .unwrap_or_default();
        HttpFetch { client }
    }

    async fn request(&self, url: &str) -> Result<reqwest::Response> {
        let resp = self.client.get(url).send().await.map_err(map_err)?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            Err(TransportError::Http(status.as_u16()))
        }
    }
}

fn io(e: std::io::Error) -> TransportError {
    TransportError::Io(e.to_string())
}

#[async_trait]
impl Fetch for HttpFetch {
    async fn get(
        &self,
        url: &str,
        dest: &Path,
        progress: &(dyn Fn(u64, Option<u64>) + Sync),
    ) -> Result<()> {
        let mut resp = self.request(url).await?;
        let total = resp.content_length();
        let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
        let mut done = 0u64;
        while let Some(chunk) = resp.chunk().await.map_err(map_err)? {
            file.write_all(&chunk).await.map_err(io)?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        file.flush().await.map_err(io)
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        self.request(url).await?.text().await.map_err(map_err)
    }
}
