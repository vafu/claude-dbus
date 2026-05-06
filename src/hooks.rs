use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, sleep, timeout};
use tracing::{info, warn};

use crate::dbus::{
    PendingRequest, SessionObject, create_session, emit_elicitation, emit_elicitation_with_id,
    emit_notification, update_session,
};
use crate::types::SessionState;
use crate::{CodexSessionParents, EndedSessions};
use agent_dbus::path::{session_key, session_path};

mod metrics;
mod permission;

#[cfg(test)]
use metrics::codex_context_pct;
use metrics::{apply_usage_limits, context_pct};
use permission::{
    build_elicitation_options, build_permission_options, build_permission_prompt,
    permission_response,
};

static NEXT_PENDING_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
const MAX_HOOK_MESSAGE_BYTES: u64 = 1024 * 1024;
const HOOK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_ELICITATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(serde::Deserialize)]
struct HookMessage {
    agent: Option<String>,
    agent_name: Option<String>,
    event: Option<String>,
    #[serde(default)]
    data: serde_json::Value,
    parent_pid: Option<u64>,
}

pub fn start_codex_compact_watcher(conn: zbus::Connection) {
    let Some(path) = codex_log_file() else {
        return;
    };

    tokio::spawn(async move {
        let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        loop {
            sleep(Duration::from_millis(500)).await;
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() < offset {
                offset = 0;
            }
            if metadata.len() == offset {
                continue;
            }

            let Ok(mut file) = std::fs::File::open(&path) else {
                continue;
            };
            if file.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut chunk = String::new();
            if file.read_to_string(&mut chunk).is_err() {
                continue;
            }
            offset = metadata.len();

            for line in chunk.lines() {
                if let Some(event) = parse_codex_compact_log_line(line) {
                    apply_codex_compact_log_event(&conn, event).await;
                }
            }
        }
    });
}

pub fn start_codex_parent_watcher(
    conn: zbus::Connection,
    ended: EndedSessions,
    codex_session_parents: CodexSessionParents,
) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(5)).await;
            let watched: Vec<(String, u32)> = codex_session_parents
                .lock()
                .await
                .iter()
                .map(|(key, pid)| (key.clone(), *pid))
                .collect();

            for (key, parent_pid) in watched {
                if process_exists(parent_pid) {
                    continue;
                }
                let Some((agent_name, session_id)) = key.split_once(':') else {
                    codex_session_parents.lock().await.remove(&key);
                    continue;
                };
                let still_current =
                    codex_session_parents.lock().await.get(&key).copied() == Some(parent_pid);
                if still_current {
                    remove_session(
                        &conn,
                        &ended,
                        &codex_session_parents,
                        agent_name,
                        session_id,
                    )
                    .await;
                    info!(
                        session_id,
                        parent_pid, "removed codex session after parent process exited"
                    );
                }
            }
        }
    });
}

fn codex_log_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".codex/log/codex-tui.log"))
}

#[derive(Debug, PartialEq)]
struct CodexCompactLogEvent {
    session_id: String,
    active: bool,
}

fn parse_codex_compact_log_line(line: &str) -> Option<CodexCompactLogEvent> {
    if !line.contains("codex.op=\"compact\"") {
        return None;
    }

    let active = if line.contains("codex_core::session::handlers: new") {
        true
    } else if line.contains("codex_core::session::handlers: close") {
        false
    } else {
        return None;
    };

    Some(CodexCompactLogEvent {
        session_id: parse_thread_id(line)?.to_string(),
        active,
    })
}

fn parse_thread_id(line: &str) -> Option<&str> {
    let start = line.find("thread_id=")? + "thread_id=".len();
    let rest = &line[start..];
    let end = rest.find('}')?;
    Some(&rest[..end])
}

async fn apply_codex_compact_log_event(conn: &zbus::Connection, event: CodexCompactLogEvent) {
    log_zbus_result(
        update_session(conn, "codex", &event.session_id, |d| {
            if event.active {
                d.state = SessionState::Compacting;
                d.task_complete = false;
                clear_pending_if_not_waiting(d);
            } else if d.state == SessionState::Compacting {
                d.state = SessionState::Idle;
                d.task_complete = true;
                clear_pending_if_not_waiting(d);
            }
        })
        .await,
        "update_session",
        &event.session_id,
    );
}

pub async fn handle_hook_connection(
    mut stream: tokio::net::UnixStream,
    conn: zbus::Connection,
    ended: EndedSessions,
    codex_session_parents: CodexSessionParents,
) {
    let mut buf = Vec::new();
    let read_result = timeout(
        HOOK_READ_TIMEOUT,
        (&mut stream)
            .take(MAX_HOOK_MESSAGE_BYTES + 1)
            .read_to_end(&mut buf),
    )
    .await;
    match read_result {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            warn!(%err, "failed to read hook message");
            return;
        }
        Err(_) => {
            warn!("timed out reading hook message");
            return;
        }
    }
    if buf.len() as u64 > MAX_HOOK_MESSAGE_BYTES {
        warn!(bytes = buf.len(), "hook message exceeded maximum size");
        return;
    }
    let raw = match std::str::from_utf8(&buf) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };

    let msg: HookMessage = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            info!("Failed to parse hook message: {}", e);
            return;
        }
    };

    let data = &msg.data;
    let event = msg.event.unwrap_or_default();
    let agent_name = msg
        .agent
        .as_deref()
        .or(msg.agent_name.as_deref())
        .or_else(|| data["agent_name"].as_str())
        .unwrap_or("agent")
        .to_string();
    let session_id = data["session_id"].as_str().unwrap_or("unknown").to_string();
    info!(agent = %agent_name, event = %event, session_id = %session_id, "hook received");
    tracing::debug!(data = %data, "hook data");

    maybe_watch_codex_parent(
        &codex_session_parents,
        &agent_name,
        &session_id,
        msg.parent_pid.and_then(|pid| u32::try_from(pid).ok()),
    )
    .await;

    match event.as_str() {
        "UpdateState" => {
            if ended
                .lock()
                .await
                .remove(&session_key(&agent_name, &session_id))
            {
                info!(session_id = %session_id, "skipping UpdateState for ended session");
                return;
            }
            let model = data["model"]["display_name"]
                .as_str()
                .or_else(|| data["model"].as_str())
                .unwrap_or("unknown")
                .to_string();
            let cwd = data["cwd"].as_str().unwrap_or("").to_string();
            let cost_usd = data["cost"]["total_cost_usd"].as_f64().unwrap_or(0.0);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    if let Some(ctx_pct) = context_pct(data) {
                        d.context_pct = ctx_pct;
                    }
                    d.model_name = model;
                    d.cwd = cwd;
                    d.cost_usd = cost_usd;
                    apply_usage_limits(d, &agent_name, &session_id, data);
                    if d.state == SessionState::NoSession {
                        d.state = SessionState::Idle;
                    }
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "SessionStart" => {
            log_zbus_result(
                create_session(&conn, &agent_name, &session_id).await,
                "create_session",
                &session_id,
            );
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Idle;
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                    d.task_complete = false;
                    clear_pending_if_not_waiting(d);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "Stop" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Idle;
                    d.task_complete = true;
                    clear_pending_if_not_waiting(d);
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "SessionEnd" => {
            remove_session(
                &conn,
                &ended,
                &codex_session_parents,
                &agent_name,
                &session_id,
            )
            .await;
        }

        "TaskCompleted" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.task_complete = true;
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "UserPromptSubmit" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Thinking;
                    d.task_complete = false;
                    clear_pending_if_not_waiting(d);
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "PreToolUse" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::ToolUse;
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "PostToolUse" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Thinking;
                    clear_pending_if_not_waiting(d);
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "Notify" | "Notification" => {
            let message = data["message"].as_str().unwrap_or("").to_string();
            let path = session_path(&agent_name, &session_id);
            if let Ok(iface_ref) = conn
                .object_server()
                .interface::<_, SessionObject>(&path)
                .await
            {
                let emitter = iface_ref.signal_emitter();
                log_zbus_result(
                    emit_notification(emitter, &message).await,
                    "emit_notification",
                    &session_id,
                );
            }
        }

        "PreCompact" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Compacting;
                    clear_pending_if_not_waiting(d);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "PermissionRequest" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
            let response = handle_elicitation_event(
                &conn,
                &agent_name,
                &session_id,
                build_permission_prompt(data),
                build_permission_options(data),
            )
            .await;
            if let Some(decision) = permission_response(data, &response) {
                if let Err(err) = stream.write_all(decision.as_bytes()).await {
                    warn!(%err, "failed to write permission response");
                }
            } else {
                info!(agent = %agent_name, session_id = %session_id, "permission request ended without an explicit response");
            }
        }

        "Elicitation" => {
            let prompt = data["elicitation"]["message"]
                .as_str()
                .or_else(|| data["message"].as_str())
                .unwrap_or("Agent needs input")
                .to_string();
            let options = build_elicitation_options(data);
            let response =
                handle_elicitation_event(&conn, &agent_name, &session_id, prompt, options).await;
            if let Err(err) = stream.write_all(response.as_bytes()).await {
                warn!(%err, "failed to write elicitation response");
            }
        }

        "RequestUserInput" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    apply_request_user_input_attention(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "RequestUserInputResolved" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    clear_request_user_input_attention(d);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        other => {
            info!("Unknown hook event: {}", other);
        }
    }
}

async fn maybe_watch_codex_parent(
    codex_session_parents: &CodexSessionParents,
    agent_name: &str,
    session_id: &str,
    parent_pid: Option<u32>,
) {
    if agent_name != "codex" || session_id == "unknown" {
        return;
    }

    let Some(parent_pid) = parent_pid else {
        return;
    };

    let key = session_key(agent_name, session_id);
    let mut parents = codex_session_parents.lock().await;
    if parents.get(&key) == Some(&parent_pid) {
        return;
    }
    parents.insert(key.clone(), parent_pid);
}

async fn remove_session(
    conn: &zbus::Connection,
    ended: &EndedSessions,
    codex_session_parents: &CodexSessionParents,
    agent_name: &str,
    session_id: &str,
) {
    let key = session_key(agent_name, session_id);
    ended.lock().await.insert(key.clone());
    codex_session_parents.lock().await.remove(&key);
    let path = session_path(agent_name, session_id);
    if let Ok(iface_ref) = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    {
        let cancelled = iface_ref.get_mut().await.cancel_pending_requests();
        if cancelled > 0 {
            info!(%session_id, cancelled, "cancelled pending requests for removed session");
        }
    }
    match conn.object_server().remove::<SessionObject, _>(&path).await {
        Ok(_) => {}
        Err(err) => warn!(%err, %session_id, "failed to remove session object"),
    }
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn log_zbus_result(result: zbus::Result<()>, action: &str, session_id: &str) {
    if let Err(err) = result {
        warn!(%err, %session_id, action, "D-Bus operation failed");
    }
}

fn model_name(data: &serde_json::Value) -> String {
    data["model"]["display_name"]
        .as_str()
        .or_else(|| data["model"].as_str())
        .unwrap_or("unknown")
        .to_string()
}

async fn handle_elicitation_event(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    prompt: String,
    options: Vec<String>,
) -> String {
    use tokio::sync::oneshot;
    info!(agent = %agent_name, session_id = %session_id, prompt = %prompt, ?options, "elicitation");

    let path = session_path(agent_name, session_id);
    if let Err(err) = conn
        .object_server()
        .at(&path, SessionObject::new(agent_name))
        .await
    {
        warn!(%err, %session_id, "failed to ensure session object for elicitation");
    }
    let iface_ref = match conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    {
        Ok(r) => r,
        Err(_) => return String::new(),
    };

    let (tx, rx) = oneshot::channel();
    let request_id = next_pending_request_id();
    {
        let mut iface = iface_ref.get_mut().await;
        iface.push_pending_request(PendingRequest {
            id: request_id.clone(),
            prompt: prompt.clone(),
            options: options.clone(),
            tx,
        });
    }

    let emitter = iface_ref.signal_emitter();
    {
        let iface = iface_ref.get().await;
        if let Err(err) = iface.emit_pending_changed(emitter).await {
            warn!(%err, %session_id, "failed to emit pending request properties");
        }
    }

    let option_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
    log_zbus_result(
        emit_elicitation(emitter, &prompt, &option_refs).await,
        "emit_elicitation",
        session_id,
    );
    log_zbus_result(
        emit_elicitation_with_id(emitter, &request_id, &prompt, &option_refs).await,
        "emit_elicitation_with_id",
        session_id,
    );

    let answer = match timeout(elicitation_timeout(), rx).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => {
            info!(agent = %agent_name, session_id = %session_id, "elicitation waiter was dropped before an explicit response");
            String::new()
        }
        Err(_) => {
            warn!(agent = %agent_name, session_id = %session_id, request_id = %request_id, "elicitation timed out");
            String::new()
        }
    };
    info!(agent = %agent_name, session_id = %session_id, answer = %answer, "elicitation answered");

    {
        let mut iface = iface_ref.get_mut().await;
        iface.remove_pending_request(&request_id);
    }
    let iface = iface_ref.get().await;
    if let Err(err) = iface.emit_pending_changed(emitter).await {
        warn!(%err, %session_id, "failed to emit pending request cleanup properties");
    }

    answer
}

fn elicitation_timeout() -> Duration {
    std::env::var("AGENT_DBUS_ELICITATION_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_ELICITATION_TIMEOUT)
}

fn clear_pending_if_not_waiting(session: &mut SessionObject) {
    if session.pending_requests.is_empty() {
        session.requires_attention = false;
    }
}

fn apply_request_user_input_attention(
    session: &mut SessionObject,
    agent_name: &str,
    session_id: &str,
    data: &serde_json::Value,
) {
    session.state = SessionState::Thinking;
    session.task_complete = false;
    session.requires_attention = true;
    session.model_name = model_name(data);
    session.cwd = data["cwd"].as_str().unwrap_or("").to_string();
    apply_usage_limits(session, agent_name, session_id, data);
}

fn clear_request_user_input_attention(session: &mut SessionObject) {
    clear_pending_if_not_waiting(session);
}

fn next_pending_request_id() -> String {
    format!(
        "req-{}",
        NEXT_PENDING_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn elicitation_options_include_always_allow_when_persisted_approval_is_supported() {
        let data = json!({
            "_meta": {
                "persist": ["session", "always"]
            },
            "elicitation": {
                "options": ["Allow", "Decline"]
            }
        });

        assert_eq!(
            build_elicitation_options(&data),
            vec!["Allow", "Always allow", "Decline"]
        );
    }

    #[test]
    fn elicitation_options_do_not_add_always_allow_without_persist_hint() {
        let data = json!({
            "elicitation": {
                "options": ["Allow", "Decline"]
            }
        });

        assert_eq!(build_elicitation_options(&data), vec!["Allow", "Decline"]);
    }

    #[test]
    fn elicitation_options_do_not_add_always_allow_to_non_approval_choices() {
        let data = json!({
            "elicitation": {
                "options": ["One", "Two"]
            }
        });

        assert_eq!(build_elicitation_options(&data), vec!["One", "Two"]);
    }

    #[test]
    fn permission_response_accepts_always_allow() {
        let data = json!({
            "permission_suggestions": [
                {
                    "type": "addRules",
                    "rules": [{ "toolName": "Bash", "ruleContent": "npm test" }],
                    "behavior": "allow",
                    "destination": "localSettings"
                }
            ]
        });
        let response: serde_json::Value =
            serde_json::from_str(&permission_response(&data, "Always allow").unwrap()).unwrap();

        assert_eq!(
            response["hookSpecificOutput"]["decision"]["updatedPermissions"][0],
            data["permission_suggestions"][0]
        );
    }

    #[test]
    fn permission_options_include_always_allow() {
        let data = json!({
            "permission_suggestions": [
                {
                    "type": "addRules",
                    "rules": [{ "toolName": "Bash", "ruleContent": "npm test" }],
                    "behavior": "allow",
                    "destination": "localSettings"
                }
            ]
        });

        assert_eq!(
            build_permission_options(&data),
            vec!["Allow", "Always allow (localSettings)", "Deny"]
        );
    }

    #[test]
    fn codex_permission_options_do_not_offer_always_allow_without_prefix_rule() {
        let data = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "transcript_path": "/tmp/codex-session.jsonl"
        });

        assert_eq!(build_permission_options(&data), vec!["Allow", "Deny"]);

        let response: serde_json::Value =
            serde_json::from_str(&permission_response(&data, "Always allow").unwrap()).unwrap();
        assert_eq!(
            response["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert!(
            response["hookSpecificOutput"]["decision"]
                .get("execPolicyAmendment")
                .is_none()
        );
    }

    #[test]
    fn codex_permission_response_uses_exec_policy_amendment() {
        let data = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "transcript_path": "/tmp/codex-session.jsonl",
            "prefix_rule": ["cargo", "build"]
        });

        let response: serde_json::Value =
            serde_json::from_str(&permission_response(&data, "Always allow").unwrap()).unwrap();

        assert_eq!(
            response["hookSpecificOutput"]["decision"]["execPolicyAmendment"],
            json!(["cargo", "build"])
        );
        assert!(
            response["hookSpecificOutput"]["decision"]
                .get("updatedPermissions")
                .is_none()
        );
    }

    #[test]
    fn permission_response_maps_specific_always_allow_option() {
        let data = json!({
            "permission_suggestions": [
                {
                    "type": "addRules",
                    "rules": [{ "toolName": "Bash", "ruleContent": "npm test" }],
                    "behavior": "allow",
                    "destination": "localSettings"
                },
                {
                    "type": "addRules",
                    "rules": [{ "toolName": "Bash", "ruleContent": "npm test" }],
                    "behavior": "allow",
                    "destination": "userSettings"
                }
            ]
        });
        let response: serde_json::Value = serde_json::from_str(
            &permission_response(&data, "Always allow (userSettings)").unwrap(),
        )
        .unwrap();

        assert_eq!(
            response["hookSpecificOutput"]["decision"]["updatedPermissions"][0],
            data["permission_suggestions"][1]
        );
    }

    #[test]
    fn codex_context_pct_uses_latest_token_count_window() {
        let payload = json!({
            "type": "token_count",
            "info": {
                "last_token_usage": {
                    "input_tokens": 100,
                    "total_tokens": 250
                },
                "model_context_window": 1000
            }
        });

        assert_eq!(codex_context_pct(&payload), Some(25.0));
    }

    #[test]
    fn context_pct_accepts_direct_hook_shapes() {
        assert_eq!(
            context_pct(&json!({
                "context_window": {
                    "used_percentage": 12.5
                }
            })),
            Some(12.5)
        );
        assert_eq!(
            context_pct(&json!({
                "context": {
                    "used_percent": 40.0
                }
            })),
            Some(40.0)
        );
    }

    #[test]
    fn parses_codex_compact_log_start_and_close() {
        let start = r#"2026-05-06T02:50:09.656315Z  INFO session_loop{thread_id=019df591-2434-7e53-bbec-94ae22260f7b}:submission_dispatch{otel.name="op.dispatch.compact" submission.id="019dfb31-5d78-7ba3-87f3-569187bb5f1f" codex.op="compact"}: codex_core::session::handlers: new"#;
        let close = r#"2026-05-06T02:50:38.077064Z  INFO session_loop{thread_id=019df591-2434-7e53-bbec-94ae22260f7b}:submission_dispatch{otel.name="op.dispatch.compact" submission.id="019dfb31-5d78-7ba3-87f3-569187bb5f1f" codex.op="compact"}: codex_core::session::handlers: close time.busy=766µs time.idle=28.4s"#;

        assert_eq!(
            parse_codex_compact_log_line(start),
            Some(CodexCompactLogEvent {
                session_id: "019df591-2434-7e53-bbec-94ae22260f7b".to_string(),
                active: true
            })
        );
        assert_eq!(
            parse_codex_compact_log_line(close),
            Some(CodexCompactLogEvent {
                session_id: "019df591-2434-7e53-bbec-94ae22260f7b".to_string(),
                active: false
            })
        );
    }

    #[test]
    fn request_user_input_attention_sets_and_clears_without_pending_requests() {
        let mut session = SessionObject::new("codex");
        let data = json!({
            "session_id": "session-1",
            "cwd": "/tmp/project",
            "model": "gpt-test"
        });

        apply_request_user_input_attention(&mut session, "codex", "session-1", &data);

        assert!(matches!(session.state, SessionState::Thinking));
        assert!(!session.task_complete);
        assert!(session.requires_attention);
        assert_eq!(session.model_name, "gpt-test");
        assert_eq!(session.cwd, "/tmp/project");
        assert_eq!(session.pending_requests.len(), 0);

        clear_request_user_input_attention(&mut session);

        assert!(!session.requires_attention);
    }
}
