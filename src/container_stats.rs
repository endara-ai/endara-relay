//! Per-container resource stats polling for containerized stdio endpoints.
//!
//! Polls `docker|podman stats --no-stream --format {{json .}} <name>` on a
//! fixed interval and stores the latest parsed sample in a shared slot that
//! the management API reads synchronously (`GET /api/endpoints` →
//! `container_stats`). Failures degrade silently: the slot is cleared and
//! polling continues — container stats never affect endpoint health.

use serde::Serialize;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use tokio::process::Command;
use tokio::time::Duration;
use tracing::debug;

/// Interval between consecutive `stats` invocations.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Hard timeout for a single `stats` invocation.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Latest resource usage sample for a containerized endpoint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerStats {
    pub cpu_percent: f64,
    pub mem_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

/// Shared slot holding the most recent stats sample. `None` means no sample
/// is available (direct spawn, poller not started yet, or last poll failed).
pub type StatsSlot = Arc<RwLock<Option<ContainerStats>>>;

/// Spawn the background poll loop for one container. The returned handle
/// must be aborted on adapter shutdown; the slot is cleared on every failed
/// poll so stale samples are never served.
pub fn spawn_stats_poller(
    runtime_cli: String,
    container_name: String,
    slot: StatsSlot,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let sample = poll_once(&runtime_cli, &container_name).await;
            if sample.is_none() {
                debug!(container = %container_name, "container stats poll failed");
            }
            if let Ok(mut guard) = slot.write() {
                *guard = sample;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
}

/// Run a single `stats --no-stream` invocation and parse its first output
/// line. Any failure (spawn error, timeout, non-zero exit, parse error)
/// returns `None` — stats are strictly best-effort.
async fn poll_once(runtime_cli: &str, container_name: &str) -> Option<ContainerStats> {
    let output = tokio::time::timeout(
        POLL_TIMEOUT,
        Command::new(runtime_cli)
            .args([
                "stats",
                "--no-stream",
                "--format",
                "{{json .}}",
                container_name,
            ])
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_stats_line(stdout.lines().next()?)
}

/// Parse one line of `stats --no-stream --format {{json .}}` output. Docker
/// and podman emit the same template keys: `CPUPerc` (e.g. `"0.05%"`),
/// `MemUsage` (e.g. `"7.293MiB / 7.667GiB"`) and `NetIO` (e.g.
/// `"1.45kB / 648B"`, rx / tx).
pub fn parse_stats_line(line: &str) -> Option<ContainerStats> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let cpu_percent = parse_percent(v.get("CPUPerc")?.as_str()?)?;
    let mem_bytes = parse_size_bytes(v.get("MemUsage")?.as_str()?.split('/').next()?)?;
    let netio = v.get("NetIO")?.as_str()?;
    let mut parts = netio.splitn(2, '/');
    let net_rx_bytes = parse_size_bytes(parts.next()?)?;
    let net_tx_bytes = parse_size_bytes(parts.next()?)?;
    Some(ContainerStats {
        cpu_percent,
        mem_bytes,
        net_rx_bytes,
        net_tx_bytes,
    })
}

/// Parse a percentage like `"0.05%"` into its numeric value.
fn parse_percent(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').trim().parse::<f64>().ok()
}

/// Parse a human-readable size like `648B`, `1.45kB`, `7.293MiB` or `2GiB`
/// into bytes. Decimal prefixes (kB/MB/GB/TB) are powers of 1000; binary
/// prefixes (KiB/MiB/GiB/TiB) are powers of 1024.
fn parse_size_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let value: f64 = s[..idx].parse().ok()?;
    let multiplier: f64 = match s[idx..].trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0_f64.powi(3),
        "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_stats_json_line() {
        let line = r#"{"BlockIO":"0B / 0B","CPUPerc":"0.05%","Container":"abc","ID":"abc","MemPerc":"0.10%","MemUsage":"7.293MiB / 7.667GiB","Name":"endara-mcp-foo","NetIO":"1.45kB / 648B","PIDs":"5"}"#;
        let stats = parse_stats_line(line).expect("docker sample should parse");
        assert_eq!(
            stats,
            ContainerStats {
                cpu_percent: 0.05,
                mem_bytes: (7.293 * 1024.0 * 1024.0) as u64,
                net_rx_bytes: 1450,
                net_tx_bytes: 648,
            }
        );
    }

    #[test]
    fn parses_podman_compat_keys_and_zero_values() {
        // Podman emits the same docker-compat template keys; a freshly
        // started container reports zeroed counters.
        let line = r#"{"ID":"deadbeef","Name":"endara-mcp-bar","CPUPerc":"0.00%","MemUsage":"0B / 0B","NetIO":"0B / 0B","BlockIO":"0B / 0B","PIDS":"1"}"#;
        let stats = parse_stats_line(line).expect("podman sample should parse");
        assert_eq!(stats.cpu_percent, 0.0);
        assert_eq!(stats.mem_bytes, 0);
        assert_eq!(stats.net_rx_bytes, 0);
        assert_eq!(stats.net_tx_bytes, 0);
    }

    #[test]
    fn rejects_non_json_and_missing_fields() {
        assert_eq!(parse_stats_line("not json"), None);
        assert_eq!(parse_stats_line(""), None);
        // Missing NetIO.
        let line = r#"{"CPUPerc":"1.00%","MemUsage":"1MiB / 1GiB"}"#;
        assert_eq!(parse_stats_line(line), None);
        // Malformed CPUPerc.
        let line = r#"{"CPUPerc":"--","MemUsage":"1MiB / 1GiB","NetIO":"0B / 0B"}"#;
        assert_eq!(parse_stats_line(line), None);
    }

    #[test]
    fn parse_size_bytes_handles_decimal_and_binary_units() {
        assert_eq!(parse_size_bytes("648B"), Some(648));
        assert_eq!(parse_size_bytes("0B"), Some(0));
        assert_eq!(parse_size_bytes("1.45kB"), Some(1450));
        assert_eq!(parse_size_bytes("2MB"), Some(2_000_000));
        assert_eq!(parse_size_bytes("3GB"), Some(3_000_000_000));
        assert_eq!(parse_size_bytes("1TB"), Some(1_000_000_000_000));
        assert_eq!(parse_size_bytes("1KiB"), Some(1024));
        assert_eq!(parse_size_bytes("1MiB"), Some(1024 * 1024));
        assert_eq!(parse_size_bytes("2GiB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1TiB"), Some(1024u64.pow(4)));
        // Whitespace tolerated (substrings of "X / Y" pairs).
        assert_eq!(parse_size_bytes(" 648B "), Some(648));
        // Bare numbers are treated as bytes.
        assert_eq!(parse_size_bytes("42"), Some(42));
        // Unknown units rejected.
        assert_eq!(parse_size_bytes("5XB"), None);
        assert_eq!(parse_size_bytes("abc"), None);
    }

    #[test]
    fn parse_percent_handles_plain_and_suffixed_values() {
        assert_eq!(parse_percent("0.05%"), Some(0.05));
        assert_eq!(parse_percent("100%"), Some(100.0));
        assert_eq!(parse_percent("12.5"), Some(12.5));
        assert_eq!(parse_percent("n/a"), None);
    }
}
