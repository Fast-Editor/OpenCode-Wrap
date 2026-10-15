use std::fs::OpenOptions;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration, Instant};

use crate::error::WrapError;
use crate::oco::{oco, OcoClient};

const HEALTH_PROBE_MS: u64 = 5_000;
const SPAWN_READY_SECS: u64 = 20;
/// The wrapper speaks the OpenCode 2 `/api/*` contract only.
pub const MIN_OPENCODE_MAJOR: u64 = 2;

pub struct SpawnState {
    pub child: Option<Child>,
    pub spawned: Arc<AtomicBool>,
}

enum Probe {
    /// `GET /api/info` answered; carries the reported server version.
    Healthy(String),
    /// Server is up but rejected our basic-auth credentials.
    Unauthorized,
    /// An OpenCode 1.x server (JSON `/global/health`, no `/api/info`).
    LegacyV1,
    /// Nothing usable is listening.
    Down,
}

async fn probe_opencode(client: &OcoClient, timeout_ms: u64) -> Probe {
    match oco(client, "/api/info", "GET", None, Some(timeout_ms)).await {
        Ok(info) => Probe::Healthy(
            info.get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        ),
        Err(WrapError::Upstream(u)) if u.upstream_status == 401 || u.upstream_status == 403 => {
            Probe::Unauthorized
        }
        Err(WrapError::Upstream(u)) if u.upstream_status == 404 => {
            match oco(client, "/global/health", "GET", None, Some(timeout_ms)).await {
                Ok(h) if h.get("healthy").and_then(|v| v.as_bool()) == Some(true) => {
                    Probe::LegacyV1
                }
                _ => Probe::Down,
            }
        }
        Err(_) => Probe::Down,
    }
}

fn unauthorized_message(client: &OcoClient) -> String {
    let base = &client.config.opencode_base;
    if client.config.opencode_password_generated {
        format!(
            "opencode serve at {base} requires a password. Set OPENCODE_SERVER_PASSWORD (and OPENCODE_SERVER_USERNAME if it is not \"opencode\") to the values that server was started with."
        )
    } else {
        format!(
            "opencode serve at {base} rejected OPENCODE_SERVER_USERNAME / OPENCODE_SERVER_PASSWORD."
        )
    }
}

fn legacy_message(client: &OcoClient) -> String {
    format!(
        "opencode serve at {} is OpenCode 1.x; this wrapper needs OpenCode {MIN_OPENCODE_MAJOR}.x (run `opencode upgrade --major`).",
        client.config.opencode_base
    )
}

fn serve_log_stdio(cwd: &str) -> Result<(Stdio, Stdio), String> {
    let path = std::path::Path::new(cwd).join("opencode-serve.log");
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let err = file
        .try_clone()
        .map_err(|e| format!("failed to dup {}: {e}", path.display()))?;
    tracing::info!("[wrap] opencode serve logs: {}", path.display());
    Ok((Stdio::from(file), Stdio::from(err)))
}

pub async fn ensure_opencode(client: &OcoClient, spawn: &mut SpawnState) -> Result<(), String> {
    match probe_opencode(client, HEALTH_PROBE_MS).await {
        Probe::Healthy(version) => {
            tracing::info!(
                "[wrap] using existing opencode serve v{} at {}",
                version,
                client.config.opencode_base
            );
            return Ok(());
        }
        Probe::Unauthorized => return Err(unauthorized_message(client)),
        Probe::LegacyV1 => return Err(legacy_message(client)),
        Probe::Down => {}
    }

    tracing::info!(
        "[wrap] spawning `opencode serve` on :{} (cwd={}) ...",
        client.config.opencode_port,
        client.config.wrap_cwd
    );
    let (stdout, stderr) = serve_log_stdio(&client.config.wrap_cwd)?;
    let child = Command::new("opencode")
        .args([
            "serve",
            "--port",
            &client.config.opencode_port.to_string(),
            "--hostname",
            "127.0.0.1",
        ])
        // OpenCode 2 protects every /api route with basic auth; hand the
        // spawned server the credentials this process will use.
        .env("OPENCODE_SERVER_USERNAME", &client.config.opencode_username)
        .env("OPENCODE_SERVER_PASSWORD", &client.config.opencode_password)
        .current_dir(&client.config.wrap_cwd)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|e| {
            format!(
                "failed to spawn `opencode serve`: {e}. Is the opencode CLI installed? See https://opencode.ai/docs"
            )
        })?;
    spawn.child = Some(child);
    spawn.spawned.store(true, Ordering::SeqCst);

    let deadline = Instant::now() + Duration::from_secs(SPAWN_READY_SECS);
    while Instant::now() < deadline {
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        if remaining_ms == 0 {
            break;
        }
        let timeout_ms = remaining_ms.clamp(1, HEALTH_PROBE_MS);
        match probe_opencode(client, timeout_ms).await {
            Probe::Healthy(version) => {
                tracing::info!("[wrap] opencode serve v{} ready", version);
                return Ok(());
            }
            Probe::Unauthorized => {
                return Err(format!(
                    "a server on {} rejected the credentials given to the spawned `opencode serve`; is another process already listening on port {}?",
                    client.config.opencode_base, client.config.opencode_port
                ));
            }
            Probe::LegacyV1 => return Err(legacy_message(client)),
            Probe::Down => {}
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "opencode serve did not become healthy in {SPAWN_READY_SECS}s"
    ))
}

/// First run of ASCII digits in `text` (e.g. `"opencode v2.0.28"` -> 2).
pub(crate) fn parse_major_version(text: &str) -> Option<u64> {
    let t = text.trim();
    let start = t.find(|c: char| c.is_ascii_digit())?;
    let digits: String = t[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

pub fn require_opencode_installed() -> Result<(), String> {
    let out = std::process::Command::new("opencode")
        .arg("--version")
        .stderr(Stdio::null())
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => {
            tracing::error!("[wrap] opencode CLI not found or not runnable.");
            tracing::error!(
                "[wrap] Install it first (see https://opencode.ai/docs), then run `opencode auth login` and restart this server."
            );
            return Err("opencode CLI missing".into());
        }
    };
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match parse_major_version(&text) {
        Some(major) if major >= MIN_OPENCODE_MAJOR => {
            tracing::info!("[wrap] opencode CLI: {}", text);
            Ok(())
        }
        Some(_) => Err(format!(
            "opencode CLI reports {text:?}; OpenCode {MIN_OPENCODE_MAJOR}.x is required (run `opencode upgrade --major`)."
        )),
        None => {
            tracing::warn!(
                "[wrap] could not parse opencode version from {:?}; continuing",
                text
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_major_version_from_cli_output() {
        assert_eq!(parse_major_version("opencode v2.0.28"), Some(2));
        assert_eq!(parse_major_version("1.19.0\n"), Some(1));
        assert_eq!(parse_major_version("opencode"), None);
    }
}
