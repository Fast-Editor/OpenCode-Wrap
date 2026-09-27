use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration};

use crate::oco::{oco, OcoClient};

pub struct SpawnState {
    pub child: Option<Child>,
    pub spawned: Arc<AtomicBool>,
}

pub async fn ensure_opencode(client: &OcoClient, spawn: &mut SpawnState) -> Result<(), String> {
    if let Ok(h) = oco(client, "/global/health", "GET", None, None).await {
        if h.get("healthy").and_then(|v| v.as_bool()) == Some(true) {
            tracing::info!(
                "[wrap] using existing opencode serve at {}",
                client.config.opencode_base
            );
            return Ok(());
        }
    }

    tracing::info!(
        "[wrap] spawning `opencode serve` on :{} (cwd={}) ...",
        client.config.opencode_port,
        client.config.wrap_cwd
    );
    let child = Command::new("opencode")
        .args([
            "serve",
            "--port",
            &client.config.opencode_port.to_string(),
            "--hostname",
            "127.0.0.1",
        ])
        .current_dir(&client.config.wrap_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            format!(
                "failed to spawn `opencode serve`: {}. Is the opencode CLI installed? See https://opencode.ai/docs",
                e
            )
        })?;
    spawn.child = Some(child);
    spawn.spawned.store(true, Ordering::SeqCst);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if let Ok(h) = oco(client, "/global/health", "GET", None, None).await {
            if h.get("healthy").and_then(|v| v.as_bool()) == Some(true) {
                tracing::info!("[wrap] opencode serve ready");
                return Ok(());
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err("opencode serve did not become healthy in 20s".into())
}

pub fn require_opencode_installed() -> Result<(), String> {
    let r = std::process::Command::new("opencode")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match r {
        Ok(s) if s.success() => Ok(()),
        _ => {
            tracing::error!("[wrap] opencode CLI not found or not runnable.");
            tracing::error!(
                "[wrap] Install it first (see https://opencode.ai/docs), then run `opencode auth login` and restart this server."
            );
            Err("opencode CLI missing".into())
        }
    }
}
