// SPDX-License-Identifier: GPL-3.0-or-later

//! `probe`: read facts from Moonraker or an SSH command on the run's host.

use super::{StepCtx, StepOutcome};
use crate::facts::Facts;
use crate::manifest::{MoonrakerProbe, Probe};
use serde_json::Value;
use std::time::Duration;

const SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const TCP_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn run(ctx: &mut StepCtx<'_>, probe: &Probe) -> StepOutcome {
    let found = match gather(ctx, probe).await {
        Ok(f) => f,
        Err(reason) => return StepOutcome::failed(reason),
    };
    match ctx.record_probe(found) {
        Ok(()) => StepOutcome::Done,
        Err(stop) => stop,
    }
}

async fn gather(ctx: &mut StepCtx<'_>, probe: &Probe) -> Result<Facts, String> {
    match probe {
        Probe::Moonraker { moonraker } => {
            let (mr, host) = (&ctx.env.moonraker, ctx.host);
            let answer = match moonraker {
                MoonrakerProbe::PrinterInfo => mr.printer_info(host).await,
                MoonrakerProbe::SystemInfo => mr.system_info(host).await,
                MoonrakerProbe::UpdateStatus => mr.update_status(host).await,
                MoonrakerProbe::ObjectsList => {
                    return Err("probing Klipper's object list is not supported".into())
                }
            };
            answer
                .map(|v| moonraker_facts(*moonraker, &v))
                .map_err(|e| format!("Moonraker did not answer: {e}"))
        }
        Probe::Ssh { ssh, fact } => {
            let session = match ctx.ssh().await {
                Ok(s) => s,
                Err(e) => return Err(format!("could not log in over SSH: {e}")),
            };
            match session.exec(ssh, None, SSH_PROBE_TIMEOUT).await {
                Ok(out) if out.exit == 0 => Ok(Facts::from([(
                    fact.clone(),
                    Value::String(out.stdout.trim().to_string()),
                )])),
                Ok(out) => Err(format!(
                    "{ssh} exited with {}: {}",
                    out.exit,
                    out.stderr.trim()
                )),
                Err(e) => {
                    ctx.drop_ssh();
                    Err(format!("{ssh} did not run: {e}"))
                }
            }
        }
        Probe::TcpPort { tcp_port, fact } => {
            let open = ctx
                .env
                .net
                .tcp_banner(ctx.host, *tcp_port, TCP_PROBE_TIMEOUT)
                .await
                .is_ok();
            Ok(Facts::from([(fact.clone(), Value::Bool(open))]))
        }
    }
}

fn str_at(v: &Value, path: &[&str]) -> Option<Value> {
    path.iter()
        .try_fold(v, |v, k| v.get(*k))
        .and_then(Value::as_str)
        .map(|s| Value::String(s.to_string()))
}

/// The facts a Moonraker answer (its `result` object) gives. Any answer at all proves Moonraker
/// is running.
pub fn moonraker_facts(kind: MoonrakerProbe, v: &Value) -> Facts {
    let mut f = Facts::from([("has_moonraker".to_string(), Value::Bool(true))]);
    let mut put = |key: &str, val: Option<Value>| {
        if let Some(val) = val {
            f.insert(key.to_string(), val);
        }
    };
    match kind {
        MoonrakerProbe::PrinterInfo => {
            put("klipper_hostname", str_at(v, &["hostname"]));
            put("klipper_state", str_at(v, &["state"]));
        }
        MoonrakerProbe::SystemInfo => {
            put(
                "cpu_arch",
                str_at(v, &["system_info", "cpu_info", "processor"]),
            );
            put(
                "os_distribution",
                str_at(v, &["system_info", "distribution", "name"]),
            );
            put(
                "services",
                v.pointer("/system_info/available_services")
                    .filter(|s| s.is_array())
                    .cloned(),
            );
        }
        MoonrakerProbe::UpdateStatus => {
            if let Some(helix) = v.pointer("/version_info/helixscreen") {
                put("helix_installed", Some(Value::Bool(true)));
                put("helix_version", str_at(helix, &["version"]));
            }
        }
        MoonrakerProbe::ObjectsList => {}
    }
    f
}

/// Whether Moonraker's `machine.system_info` answer shows the HelixScreen service active;
/// `None` when it lists no such service (init.d printers, or not installed yet).
pub fn helix_service_active(system_info: &Value) -> Option<bool> {
    system_info
        .pointer("/system_info/service_state/helixscreen")
        .map(|s| s.get("active_state").and_then(Value::as_str) == Some("active"))
}
