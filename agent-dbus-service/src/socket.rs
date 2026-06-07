use std::time::Duration;

use agent_dbus_core::provider::RawHook;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tracing::{info, warn};

const MAX_HOOK_MESSAGE_BYTES: u64 = 1024 * 1024;
const HOOK_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(serde::Deserialize)]
struct HookMessage {
    agent: Option<String>,
    agent_name: Option<String>,
    event: Option<String>,
    #[serde(default)]
    data: serde_json::Value,
    parent_pid: Option<u64>,
    app_instance_id: Option<String>,
    window_id: Option<String>,
}

pub(crate) async fn read_raw_hook(stream: &mut tokio::net::UnixStream) -> Option<RawHook> {
    let mut buf = Vec::new();
    let read_result = timeout(
        HOOK_READ_TIMEOUT,
        stream
            .take(MAX_HOOK_MESSAGE_BYTES + 1)
            .read_to_end(&mut buf),
    )
    .await;
    match read_result {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            warn!(%err, "failed to read hook message");
            return None;
        }
        Err(_) => {
            warn!("timed out reading hook message");
            return None;
        }
    }
    if buf.len() as u64 > MAX_HOOK_MESSAGE_BYTES {
        warn!(bytes = buf.len(), "hook message exceeded maximum size");
        return None;
    }
    let raw = match std::str::from_utf8(&buf) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return None,
    };

    let msg: HookMessage = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            info!("Failed to parse hook message: {}", e);
            return None;
        }
    };

    let agent = msg
        .agent
        .as_deref()
        .or(msg.agent_name.as_deref())
        .or_else(|| msg.data["agent_name"].as_str())
        .unwrap_or("agent")
        .to_string();
    let session_id = msg.data["session_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    Some(RawHook {
        agent,
        event: msg.event.unwrap_or_default(),
        session_id,
        data: msg.data,
        parent_pid: msg.parent_pid.and_then(|pid| u32::try_from(pid).ok()),
        app_instance_id: non_empty_string(msg.app_instance_id.as_deref()),
        window_id: non_empty_string(msg.window_id.as_deref()),
    })
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}
