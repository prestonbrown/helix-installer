// SPDX-License-Identifier: GPL-3.0-or-later

//! `Net` over real TCP sockets.

use super::{Host, Net, Result, TransportError};
use async_trait::async_trait;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout as within, Instant};

/// Longest banner line read; SSH caps its identification line at 255 bytes.
const MAX_BANNER: u64 = 256;

pub struct TcpNet;

#[async_trait]
impl Net for TcpNet {
    async fn tcp_banner(
        &self,
        host: &Host,
        port: u16,
        timeout: Duration,
    ) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        let stream = match within(timeout, TcpStream::connect((host.addr.as_str(), port))).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) | Err(_) => return Err(TransportError::Unreachable),
        };
        let mut line = String::new();
        let mut reader = BufReader::new(stream.take(MAX_BANNER));
        let left = deadline.saturating_duration_since(Instant::now());
        // Connected is the answer; a server that is silent, hangs up or sends garbage still
        // proved the port is open.
        match within(left, reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => Ok(Some(line.trim_end().to_string())),
            _ => Ok(None),
        }
    }
}
