use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, sleep, timeout};
use tracing::{info, warn};

use crate::dbus::{
    PendingRequest, SessionObject, create_session, emit_elicitation, emit_elicitation_with_details,
    emit_elicitation_with_id, emit_elicitation_with_id_and_details, emit_notification,
    update_session,
};
use crate::types::SessionState;
use crate::{CodexSessionParents, EndedSessions};
use agent_dbus::agent::is_gemini_agent;
use agent_dbus::path::{agent_session_node_key, safe_path_segment, session_key, session_path};
use locus::{GraphReadProxy, GraphWriteProxy};

mod metrics;
mod permission;

#[cfg(test)]
use metrics::codex_context_pct;
use metrics::{apply_usage_limits, codex_session_file, context_pct};
use permission::{
    build_elicitation_options, build_permission_detail, build_permission_option_descriptions,
    build_permission_options, build_permission_prompt, permission_response,
    should_defer_codex_permission_to_auto_review,
};

const SUBAGENT_SESSION_RELATION: &str = "subagent-session";
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
    locus_app_instance: Option<String>,
    locus_window_id: Option<String>,
}

struct ElicitationRequest {
    prompt: String,
    detail_kind: String,
    detail_text: String,
    options: Vec<String>,
    option_descriptions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct SubagentInfo {
    parent_session_id: String,
    nickname: String,
    role: String,
}

#[derive(Clone, Debug, Default)]
struct LocusContext {
    app_instance: Option<String>,
    window_id: Option<String>,
}

impl LocusContext {
    fn from_message(msg: &HookMessage) -> Self {
        Self {
            app_instance: non_empty_string(msg.locus_app_instance.as_deref()),
            window_id: non_empty_string(msg.locus_window_id.as_deref()),
        }
    }

    fn app_instance_for(&self, agent_name: &str, session_id: &str) -> Option<String> {
        self.app_instance.clone().or_else(|| {
            self.window_id.as_ref().map(|_| {
                format!(
                    "app-instance:{}",
                    agent_session_node_key(agent_name, session_id)
                )
            })
        })
    }
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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
    let event = msg.event.clone().unwrap_or_default();
    let agent_name = msg
        .agent
        .as_deref()
        .or(msg.agent_name.as_deref())
        .or_else(|| data["agent_name"].as_str())
        .unwrap_or("agent")
        .to_string();
    let session_id = data["session_id"].as_str().unwrap_or("unknown").to_string();
    let locus_context = LocusContext::from_message(&msg);
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
                    d.model_name = model.clone();
                    d.cwd = cwd.clone();
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
            publish_locus_agent_session_link(
                &agent_name,
                &session_id,
                &locus_context,
                data["cwd"].as_str(),
                Some(&model),
            )
            .await;
        }

        "SessionStart" => {
            let subagent_info = codex_subagent_info(&agent_name, &session_id, data);
            let model = model_name(data);
            log_zbus_result(
                create_session(&conn, &agent_name, &session_id).await,
                "create_session",
                &session_id,
            );
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Idle;
                    d.model_name = model.clone();
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                    apply_subagent_info(d, subagent_info.as_ref());
                    d.task_complete = false;
                    clear_pending_if_not_waiting(d);
                })
                .await,
                "update_session",
                &session_id,
            );
            if let Some(info) = subagent_info.as_ref() {
                publish_locus_subagent_session_link(
                    &agent_name,
                    &session_id,
                    info,
                    data["cwd"].as_str(),
                    false,
                )
                .await;
            } else {
                publish_locus_agent_session_link(
                    &agent_name,
                    &session_id,
                    &locus_context,
                    data["cwd"].as_str(),
                    Some(&model),
                )
                .await;
            }
        }

        "Stop" | "AfterAgent" => {
            let subagent_info = codex_subagent_info(&agent_name, &session_id, data);
            let parent_session_id = if let Some(info) = subagent_info.as_ref() {
                Some(info.parent_session_id.clone())
            } else {
                subagent_parent_session_id(&conn, &agent_name, &session_id).await
            };
            if parent_session_id.is_some() {
                let fallback_info;
                let info = if let Some(info) = subagent_info.as_ref() {
                    info
                } else {
                    fallback_info = SubagentInfo {
                        parent_session_id: parent_session_id.unwrap_or_default(),
                        nickname: String::new(),
                        role: String::new(),
                    };
                    &fallback_info
                };
                publish_locus_subagent_session_link(
                    &agent_name,
                    &session_id,
                    info,
                    data["cwd"].as_str(),
                    true,
                )
                .await;
                remove_session(
                    &conn,
                    &ended,
                    &codex_session_parents,
                    &agent_name,
                    &session_id,
                )
                .await;
                info!(agent = %agent_name, session_id = %session_id, "removed subagent session after Stop");
                return;
            }
            let model = model_name(data);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Idle;
                    d.task_complete = true;
                    clear_pending_if_not_waiting(d);
                    d.model_name = model.clone();
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
            publish_locus_agent_session_link(
                &agent_name,
                &session_id,
                &locus_context,
                data["cwd"].as_str(),
                Some(&model),
            )
            .await;
        }

        "SessionEnd" => {
            remove_locus_agent_session_link(&agent_name, &session_id, &locus_context).await;
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

        "UserPromptSubmit" | "BeforeAgent" | "BeforeModel" | "BeforeToolSelection" => {
            let subagent_info = codex_subagent_info(&agent_name, &session_id, data);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Thinking;
                    d.task_complete = false;
                    clear_pending_if_not_waiting(d);
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                    apply_subagent_info(d, subagent_info.as_ref());
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "AfterModel" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Thinking;
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "BeforeTool" if is_gemini_agent(&agent_name) => {
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
            let options = build_permission_options(data);
            let option_descriptions = build_permission_option_descriptions(data, &options);
            let detail = build_permission_detail(data);
            let response = handle_elicitation_event(
                &conn,
                &agent_name,
                &session_id,
                ElicitationRequest {
                    prompt: build_permission_prompt(data),
                    detail_kind: detail.kind,
                    detail_text: detail.text,
                    options,
                    option_descriptions,
                },
            )
            .await;
            if let Some(decision) = permission_response(&agent_name, data, &response) {
                if let Err(err) = stream.write_all(decision.as_bytes()).await {
                    warn!(%err, "failed to write gemini tool response");
                }
            } else {
                info!(agent = %agent_name, session_id = %session_id, "gemini tool request ended without an explicit response");
            }
        }

        "PreToolUse" | "BeforeTool" => {
            let subagent_info = codex_subagent_info(&agent_name, &session_id, data);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::ToolUse;
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                    apply_subagent_info(d, subagent_info.as_ref());
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "PostToolUse" | "AfterTool" => {
            let subagent_info = codex_subagent_info(&agent_name, &session_id, data);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.state = SessionState::Thinking;
                    clear_transient_attention(d);
                    d.model_name = model_name(data);
                    d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                    apply_usage_limits(d, &agent_name, &session_id, data);
                    apply_subagent_info(d, subagent_info.as_ref());
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

        "PreCompact" | "PreCompress" => {
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
            if should_defer_codex_permission_to_auto_review(&agent_name, data) {
                info!(agent = %agent_name, session_id = %session_id, "deferred codex permission request to auto-review");
                return;
            }
            let options = build_permission_options(data);
            let option_descriptions = build_permission_option_descriptions(data, &options);
            let detail = build_permission_detail(data);
            let response = handle_elicitation_event(
                &conn,
                &agent_name,
                &session_id,
                ElicitationRequest {
                    prompt: build_permission_prompt(data),
                    detail_kind: detail.kind,
                    detail_text: detail.text,
                    options,
                    option_descriptions,
                },
            )
            .await;
            if let Some(decision) = permission_response(&agent_name, data, &response) {
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
            let option_descriptions = vec![String::new(); options.len()];
            let response = handle_elicitation_event(
                &conn,
                &agent_name,
                &session_id,
                ElicitationRequest {
                    prompt,
                    detail_kind: String::new(),
                    detail_text: String::new(),
                    options,
                    option_descriptions,
                },
            )
            .await;
            if let Err(err) = stream.write_all(response.as_bytes()).await {
                warn!(%err, "failed to write elicitation response");
            }
        }

        "RequestUserInput" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    apply_nonblocking_attention(
                        d,
                        &agent_name,
                        &session_id,
                        data,
                        "request-user-input",
                        SessionState::Thinking,
                        false,
                    );
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "RequestUserInputResolved" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.clear_attention_reason("request-user-input");
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "PlanModePrompt" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    apply_nonblocking_attention(
                        d,
                        &agent_name,
                        &session_id,
                        data,
                        "plan-mode-prompt",
                        SessionState::Idle,
                        true,
                    );
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "PlanModePromptResolved" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.clear_attention_reason("plan-mode-prompt");
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "AgentTurnCompleteAttention" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    apply_nonblocking_attention(
                        d,
                        &agent_name,
                        &session_id,
                        data,
                        "agent-turn-complete",
                        SessionState::Idle,
                        true,
                    );
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "AgentTurnCompleteAttentionResolved" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.clear_attention_reason("agent-turn-complete");
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "ToolSuggestion" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    apply_nonblocking_attention(
                        d,
                        &agent_name,
                        &session_id,
                        data,
                        "tool-suggestion",
                        SessionState::Idle,
                        false,
                    );
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "ToolSuggestionResolved" => {
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.clear_attention_reason("tool-suggestion");
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "ExecApprovalRequest" => {
            apply_attention_event(
                &conn,
                &agent_name,
                &session_id,
                data,
                "exec-approval",
                SessionState::Thinking,
                false,
            )
            .await;
        }

        "ExecApprovalResolved" => {
            clear_attention_event(&conn, &agent_name, &session_id, "exec-approval").await;
        }

        "ApplyPatchApprovalRequest" => {
            apply_attention_event(
                &conn,
                &agent_name,
                &session_id,
                data,
                "apply-patch-approval",
                SessionState::Thinking,
                false,
            )
            .await;
        }

        "ApplyPatchApprovalResolved" => {
            clear_attention_event(&conn, &agent_name, &session_id, "apply-patch-approval").await;
        }

        "RequestPermissions" => {
            apply_attention_event(
                &conn,
                &agent_name,
                &session_id,
                data,
                "request-permissions",
                SessionState::Thinking,
                false,
            )
            .await;
        }

        "RequestPermissionsResolved" => {
            clear_attention_event(&conn, &agent_name, &session_id, "request-permissions").await;
        }

        "McpServerElicitationRequest" => {
            apply_attention_event(
                &conn,
                &agent_name,
                &session_id,
                data,
                "mcp-server-elicitation",
                SessionState::Thinking,
                false,
            )
            .await;
        }

        "McpServerElicitationResolved" => {
            clear_attention_event(&conn, &agent_name, &session_id, "mcp-server-elicitation").await;
        }

        "AttentionRequired" => {
            let reason = attention_reason(data);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    apply_nonblocking_attention(
                        d,
                        &agent_name,
                        &session_id,
                        data,
                        &reason,
                        attention_state(data).unwrap_or(SessionState::Idle),
                        data["task_complete"].as_bool().unwrap_or(d.task_complete),
                    );
                })
                .await,
                "update_session",
                &session_id,
            );
        }

        "AttentionResolved" => {
            let reason = attention_reason(data);
            log_zbus_result(
                update_session(&conn, &agent_name, &session_id, |d| {
                    d.clear_attention_reason(&reason);
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
    remove_locus_agent_session_link(agent_name, session_id, &LocusContext::default()).await;
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

async fn subagent_parent_session_id(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
) -> Option<String> {
    let path = session_path(agent_name, session_id);
    let Ok(iface_ref) = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    else {
        return None;
    };
    let iface = iface_ref.get().await;
    (iface.is_subagent && !iface.parent_session_id.is_empty())
        .then(|| iface.parent_session_id.clone())
}

async fn publish_locus_agent_session_link(
    agent_name: &str,
    session_id: &str,
    context: &LocusContext,
    cwd: Option<&str>,
    model: Option<&str>,
) {
    let Ok(connection) = zbus::Connection::session().await else {
        return;
    };
    let Ok(locus_write) = GraphWriteProxy::new(&connection).await else {
        return;
    };

    let target = format!(
        "agent-session:{}",
        agent_session_node_key(agent_name, session_id)
    );
    let _ = locus_write
        .set_property(&target, "kind", "agent-session")
        .await;
    let _ = locus_write.set_property(&target, "id", session_id).await;
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        let _ = locus_write.set_property(&target, "cwd", cwd).await;
    }
    if let Some(model) = model.filter(|model| !model.is_empty() && *model != "unknown") {
        let _ = locus_write.set_property(&target, "model", model).await;
    }
    publish_session_project(&locus_write, &target, cwd).await;

    let Some(app_instance) = context.app_instance_for(agent_name, session_id) else {
        return;
    };
    if let Some(window_id) = context.window_id.as_deref() {
        let window = format!("window:{window_id}");
        let _ = locus_write
            .set_property(&app_instance, "kind", "app-instance")
            .await;
        let _ = locus_write
            .set_property(&app_instance, "name", agent_name)
            .await;
        let _ = locus_write
            .set_property(&app_instance, "icon", &safe_path_segment(agent_name))
            .await;
        let _ = locus_write
            .set_link(&window, "app-instance", &app_instance)
            .await;
    }
    let _ = locus_write
        .set_link(&app_instance, "agent-session", &target)
        .await;
}

async fn remove_locus_agent_session_link(
    agent_name: &str,
    session_id: &str,
    context: &LocusContext,
) {
    let Ok(connection) = zbus::Connection::session().await else {
        return;
    };
    let Ok(locus_write) = GraphWriteProxy::new(&connection).await else {
        return;
    };
    let Ok(locus_read) = GraphReadProxy::new(&connection).await else {
        return;
    };

    let target = format!(
        "agent-session:{}",
        agent_session_node_key(agent_name, session_id)
    );
    remove_existing_top_level_session_links(&locus_write, &locus_read, &target).await;
    let _ = locus_write.remove_links(&target, "session-project").await;
    if let Some(app_instance) = context.app_instance_for(agent_name, session_id)
        && let Some(window_id) = context.window_id.as_deref()
    {
        let window = format!("window:{window_id}");
        let _ = locus_write
            .remove_link(&window, "app-instance", &app_instance)
            .await;
    }
    let _ = locus_write.delete_node(&target).await;
}

async fn remove_existing_top_level_session_links(
    locus: &GraphWriteProxy<'_>,
    locus_read: &GraphReadProxy<'_>,
    target: &str,
) {
    for app_instance in locus_read
        .get_sources(target, "agent-session")
        .await
        .unwrap_or_default()
    {
        let _ = locus
            .remove_link(&app_instance, "agent-session", target)
            .await;
    }
}

async fn publish_session_project(locus: &GraphWriteProxy<'_>, session: &str, cwd: Option<&str>) {
    let Some(project) = cwd.and_then(project_for_cwd) else {
        return;
    };
    let subject = format!("project:{}", project.root.display());

    let _ = locus.set_property(&subject, "kind", "project").await;
    let _ = locus
        .set_property(&subject, "path", &project.root.display().to_string())
        .await;
    let _ = locus.set_property(&subject, "name", &project.name).await;
    if let Some(icon) = project.icon.as_deref().filter(|icon| !icon.is_empty()) {
        let _ = locus.set_property(&subject, "icon", icon).await;
    }
    let _ = locus.set_link(session, "session-project", &subject).await;
}

struct Project {
    root: PathBuf,
    name: String,
    icon: Option<String>,
}

fn project_for_cwd(cwd: &str) -> Option<Project> {
    let cwd = std::fs::canonicalize(cwd).ok()?;
    let parent = project_parent()?;
    let relative = cwd.strip_prefix(&parent).ok()?;
    let project_name = relative.components().next()?.as_os_str().to_str()?;
    if project_name.is_empty() {
        return None;
    }

    let root = parent.join(project_name);
    let metadata = read_project_metadata(&root);
    Some(Project {
        root,
        name: metadata
            .as_ref()
            .and_then(|value| json_string(value, "name"))
            .unwrap_or_else(|| project_name.to_string()),
        icon: metadata
            .as_ref()
            .and_then(|value| json_string(value, "icon")),
    })
}

fn project_parent() -> Option<PathBuf> {
    let parent = std::env::var_os("PROJECT_PARENT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join("proj")))?;
    std::fs::canonicalize(parent).ok()
}

fn read_project_metadata(root: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(root.join(".project.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

async fn publish_locus_subagent_session_link(
    agent_name: &str,
    session_id: &str,
    info: &SubagentInfo,
    cwd: Option<&str>,
    remove: bool,
) {
    let Ok(connection) = zbus::Connection::session().await else {
        return;
    };
    let Ok(locus_write) = GraphWriteProxy::new(&connection).await else {
        return;
    };
    let Ok(locus_read) = GraphReadProxy::new(&connection).await else {
        return;
    };

    let target = format!(
        "agent-session:{}",
        agent_session_node_key(agent_name, session_id)
    );
    let parent = format!(
        "agent-session:{}",
        agent_session_node_key(agent_name, &info.parent_session_id)
    );

    for app_instance in locus_read
        .get_sources(&target, "agent-session")
        .await
        .unwrap_or_default()
    {
        let _ = locus_write
            .remove_link(&app_instance, "agent-session", &target)
            .await;
    }

    if remove {
        let _ = locus_write
            .remove_link(&parent, SUBAGENT_SESSION_RELATION, &target)
            .await;
        let _ = locus_write.remove_links(&target, "session-project").await;
        return;
    }

    let _ = locus_write
        .set_property(&target, "kind", "agent-session")
        .await;
    let _ = locus_write.set_property(&target, "id", session_id).await;
    if let Some(cwd) = cwd {
        let _ = locus_write.set_property(&target, "cwd", cwd).await;
    }
    let _ = locus_write
        .set_property(&parent, "kind", "agent-session")
        .await;
    let _ = locus_write
        .set_property(&parent, "id", &info.parent_session_id)
        .await;
    let _ = locus_write
        .set_link(&parent, SUBAGENT_SESSION_RELATION, &target)
        .await;
    let _ = locus_write.remove_links(&target, "session-project").await;
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn log_zbus_result(result: zbus::Result<()>, action: &str, session_id: &str) {
    if let Err(err) = result {
        warn!(%err, %session_id, action, "D-Bus operation failed");
    }
}

async fn apply_attention_event(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    data: &serde_json::Value,
    reason: &str,
    state: SessionState,
    task_complete: bool,
) {
    log_zbus_result(
        update_session(conn, agent_name, session_id, |d| {
            apply_nonblocking_attention(
                d,
                agent_name,
                session_id,
                data,
                reason,
                state,
                task_complete,
            );
        })
        .await,
        "update_session",
        session_id,
    );
}

async fn clear_attention_event(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    reason: &str,
) {
    log_zbus_result(
        update_session(conn, agent_name, session_id, |d| {
            d.clear_attention_reason(reason);
        })
        .await,
        "update_session",
        session_id,
    );
}

fn model_name(data: &serde_json::Value) -> String {
    data["model"]["display_name"]
        .as_str()
        .or_else(|| data["model"].as_str())
        .or_else(|| data["llm_request"]["model"].as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn apply_subagent_info(session: &mut SessionObject, info: Option<&SubagentInfo>) {
    let Some(info) = info else {
        return;
    };
    session.is_subagent = true;
    session.parent_session_id = info.parent_session_id.clone();
    session.agent_nickname = info.nickname.clone();
    session.agent_role = info.role.clone();
}

fn codex_subagent_info(
    agent_name: &str,
    session_id: &str,
    data: &serde_json::Value,
) -> Option<SubagentInfo> {
    if agent_name != "codex" {
        return None;
    }

    subagent_info_from_value(data)
        .or_else(|| codex_session_meta_from_hook(data))
        .or_else(|| {
            codex_session_file(session_id).and_then(|path| codex_session_meta_from_path(&path))
        })
        .and_then(|meta| {
            if meta.parent_session_id.is_empty() {
                None
            } else {
                Some(meta)
            }
        })
}

fn subagent_info_from_value(value: &serde_json::Value) -> Option<SubagentInfo> {
    let thread_source = json_string_at(value, &["thread_source"])
        .or_else(|| json_string_at(value, &["payload", "thread_source"]));
    let explicit_subagent = thread_source.as_deref() == Some("subagent")
        || value
            .pointer("/source/subagent/thread_spawn")
            .is_some_and(serde_json::Value::is_object)
        || value
            .pointer("/payload/source/subagent/thread_spawn")
            .is_some_and(serde_json::Value::is_object);

    let parent_session_id = json_string_at(
        value,
        &["source", "subagent", "thread_spawn", "parent_thread_id"],
    )
    .or_else(|| {
        json_string_at(
            value,
            &[
                "payload",
                "source",
                "subagent",
                "thread_spawn",
                "parent_thread_id",
            ],
        )
    })
    .or_else(|| json_string_at(value, &["thread_spawn", "parent_thread_id"]))
    .or_else(|| json_string_at(value, &["parent_thread_id"]))
    .unwrap_or_default();

    if !explicit_subagent && parent_session_id.is_empty() {
        return None;
    }

    Some(SubagentInfo {
        parent_session_id,
        nickname: json_string_at(value, &["agent_nickname"])
            .or_else(|| json_string_at(value, &["payload", "agent_nickname"]))
            .unwrap_or_default(),
        role: json_string_at(value, &["agent_role"])
            .or_else(|| json_string_at(value, &["payload", "agent_role"]))
            .unwrap_or_default(),
    })
}

fn codex_session_meta_from_hook(data: &serde_json::Value) -> Option<SubagentInfo> {
    let path = data["transcript_path"]
        .as_str()
        .or_else(|| data["session_path"].as_str())
        .or_else(|| data["session_file"].as_str())?;
    codex_session_meta_from_path(Path::new(path))
}

fn codex_session_meta_from_path(path: &Path) -> Option<SubagentInfo> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut contents = String::new();
    (&mut file)
        .take(256 * 1024)
        .read_to_string(&mut contents)
        .ok()?;
    contents.lines().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        if entry["type"].as_str()? != "session_meta" {
            return None;
        }
        subagent_info_from_value(&entry["payload"])
    })
}

fn json_string_at(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_str().map(str::to_string)
}

async fn handle_elicitation_event(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    request: ElicitationRequest,
) -> String {
    use tokio::sync::oneshot;
    info!(agent = %agent_name, session_id = %session_id, prompt = %request.prompt, ?request.options, "elicitation");

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
        if !iface.push_pending_request(PendingRequest {
            id: request_id.clone(),
            prompt: request.prompt.clone(),
            detail_kind: request.detail_kind,
            detail_text: request.detail_text,
            options: request.options.clone(),
            option_descriptions: request.option_descriptions.clone(),
            tx,
        }) {
            warn!(
                agent = %agent_name,
                session_id = %session_id,
                "pending request limit reached; dropping elicitation"
            );
            return String::new();
        }
    }

    let emitter = iface_ref.signal_emitter();
    {
        let iface = iface_ref.get().await;
        if let Err(err) = iface.emit_pending_changed(emitter).await {
            warn!(%err, %session_id, "failed to emit pending request properties");
        }
    }

    let option_refs: Vec<&str> = request.options.iter().map(|s| s.as_str()).collect();
    let option_description_refs: Vec<&str> = request
        .option_descriptions
        .iter()
        .map(|s| s.as_str())
        .collect();
    log_zbus_result(
        emit_elicitation(emitter, &request.prompt, &option_refs).await,
        "emit_elicitation",
        session_id,
    );
    log_zbus_result(
        emit_elicitation_with_id(emitter, &request_id, &request.prompt, &option_refs).await,
        "emit_elicitation_with_id",
        session_id,
    );
    log_zbus_result(
        emit_elicitation_with_details(
            emitter,
            &request.prompt,
            &option_refs,
            &option_description_refs,
        )
        .await,
        "emit_elicitation_with_details",
        session_id,
    );
    log_zbus_result(
        emit_elicitation_with_id_and_details(
            emitter,
            &request_id,
            &request.prompt,
            &option_refs,
            &option_description_refs,
        )
        .await,
        "emit_elicitation_with_id_and_details",
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
    session.clear_attention_reasons();
}

fn clear_transient_attention(session: &mut SessionObject) {
    session.clear_attention_reasons();
}

fn apply_nonblocking_attention(
    session: &mut SessionObject,
    agent_name: &str,
    session_id: &str,
    data: &serde_json::Value,
    reason: &str,
    state: SessionState,
    task_complete: bool,
) {
    session.state = state;
    session.task_complete = task_complete;
    session.set_attention_reason(reason);
    session.model_name = model_name(data);
    session.cwd = data["cwd"].as_str().unwrap_or("").to_string();
    apply_usage_limits(session, agent_name, session_id, data);
}

fn attention_reason(data: &serde_json::Value) -> String {
    data["reason"]
        .as_str()
        .or_else(|| data["kind"].as_str())
        .or_else(|| data["attention_kind"].as_str())
        .unwrap_or("attention")
        .to_string()
}

fn attention_state(data: &serde_json::Value) -> Option<SessionState> {
    match data["state"].as_str()? {
        "no-session" => Some(SessionState::NoSession),
        "idle" => Some(SessionState::Idle),
        "thinking" => Some(SessionState::Thinking),
        "tool-use" => Some(SessionState::ToolUse),
        "compacting" => Some(SessionState::Compacting),
        _ => None,
    }
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
            serde_json::from_str(&permission_response("claude", &data, "Always allow").unwrap())
                .unwrap();

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
    fn permission_option_descriptions_explain_always_allow_suggestion() {
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
        let options = build_permission_options(&data);

        assert_eq!(
            build_permission_option_descriptions(&data, &options),
            vec![
                "Allow only this request.",
                "Persistently allow: Bash npm test Destination: localSettings.",
                "Deny this request."
            ]
        );
    }

    #[test]
    fn codex_apply_patch_permission_detail_preserves_patch() {
        let patch =
            "*** Begin Patch\n*** Update File: example.txt\n@@\n-old\n+new\n*** End Patch\n";
        let data = json!({
            "tool_name": "apply_patch",
            "tool_input": { "command": patch }
        });

        let detail = build_permission_detail(&data);

        assert_eq!(detail.kind, "diff");
        assert_eq!(detail.text, patch);
    }

    #[test]
    fn claude_edit_permission_detail_builds_diff() {
        let data = json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "/tmp/example.txt",
                "old_string": "old line\nsame",
                "new_string": "new line\nsame"
            }
        });

        let detail = build_permission_detail(&data);

        assert_eq!(detail.kind, "diff");
        assert_eq!(
            detail.text,
            "--- /tmp/example.txt\n+++ /tmp/example.txt\n@@\n-old line\n-same\n+new line\n+same\n"
        );
    }

    #[test]
    fn shell_permission_detail_preserves_full_command() {
        let command = "printf 'line 1'\nprintf 'line 2'\nprintf 'line 3'";
        let data = json!({
            "tool_name": "Bash",
            "tool_input": { "command": command }
        });

        let detail = build_permission_detail(&data);

        assert_eq!(detail.kind, "command");
        assert_eq!(detail.text, command);
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
            serde_json::from_str(&permission_response("codex", &data, "Always allow").unwrap())
                .unwrap();
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
    fn codex_permission_request_defers_to_auto_review() {
        let data = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "transcript_path": "/tmp/codex-session.jsonl",
            "approvals_reviewer": "auto_review"
        });

        assert!(should_defer_codex_permission_to_auto_review("codex", &data));
    }

    #[test]
    fn codex_permission_request_shows_auto_review_denied_fallback() {
        let data = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "transcript_path": "/tmp/codex-session.jsonl",
            "approvals_reviewer": "auto_review",
            "auto_review": {
                "status": "denied"
            }
        });

        assert!(!should_defer_codex_permission_to_auto_review(
            "codex", &data
        ));
    }

    #[test]
    fn codex_permission_request_does_not_defer_user_reviewer() {
        let data = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "transcript_path": "/tmp/codex-session.jsonl",
            "approvals_reviewer": "user"
        });

        assert!(!should_defer_codex_permission_to_auto_review(
            "codex", &data
        ));
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
            serde_json::from_str(&permission_response("codex", &data, "Always allow").unwrap())
                .unwrap();

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
    fn codex_permission_option_descriptions_explain_prefix_rule() {
        let data = json!({
            "hook_event_name": "PermissionRequest",
            "permission_mode": "default",
            "transcript_path": "/tmp/codex-session.jsonl",
            "prefix_rule": ["rm", "-rf", "/tmp/example"]
        });
        let options = build_permission_options(&data);

        assert_eq!(
            build_permission_option_descriptions(&data, &options),
            vec![
                "Allow only this request.",
                "Persistently allow commands starting with: rm -rf /tmp/example",
                "Deny this request."
            ]
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
            &permission_response("claude", &data, "Always allow (userSettings)").unwrap(),
        )
        .unwrap();

        assert_eq!(
            response["hookSpecificOutput"]["decision"]["updatedPermissions"][0],
            data["permission_suggestions"][1]
        );
    }

    #[test]
    fn gemini_permission_response_uses_gemini_decision_shape() {
        let allow: serde_json::Value =
            serde_json::from_str(&permission_response("gemini", &json!({}), "Allow").unwrap())
                .unwrap();
        assert_eq!(allow, json!({ "decision": "allow" }));

        let deny: serde_json::Value =
            serde_json::from_str(&permission_response("gemini-cli", &json!({}), "Deny").unwrap())
                .unwrap();
        assert_eq!(
            deny,
            json!({
                "decision": "deny",
                "reason": "User denied via popup"
            })
        );
    }

    #[test]
    fn model_name_accepts_gemini_llm_request_shape() {
        assert_eq!(
            model_name(&json!({
                "llm_request": {
                    "model": "gemini-3-pro"
                }
            })),
            "gemini-3-pro"
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
    fn subagent_info_parses_codex_session_meta_payload() {
        let data = json!({
            "id": "child-session",
            "thread_source": "subagent",
            "agent_nickname": "Helmholtz",
            "agent_role": "explorer",
            "source": {
                "subagent": {
                    "thread_spawn": {
                        "parent_thread_id": "parent-session",
                        "depth": 1
                    }
                }
            }
        });

        assert_eq!(
            subagent_info_from_value(&data),
            Some(SubagentInfo {
                parent_session_id: "parent-session".to_string(),
                nickname: "Helmholtz".to_string(),
                role: "explorer".to_string(),
            })
        );
    }

    #[test]
    fn subagent_info_parses_nested_payload_shape() {
        let data = json!({
            "payload": {
                "thread_source": "subagent",
                "agent_nickname": "Locke",
                "agent_role": "worker",
                "source": {
                    "subagent": {
                        "thread_spawn": {
                            "parent_thread_id": "parent-session"
                        }
                    }
                }
            }
        });

        assert_eq!(
            subagent_info_from_value(&data),
            Some(SubagentInfo {
                parent_session_id: "parent-session".to_string(),
                nickname: "Locke".to_string(),
                role: "worker".to_string(),
            })
        );
    }

    #[test]
    fn subagent_info_ignores_regular_sessions() {
        let data = json!({
            "thread_source": "thread",
            "agent_nickname": "main"
        });

        assert_eq!(subagent_info_from_value(&data), None);
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

        apply_nonblocking_attention(
            &mut session,
            "codex",
            "session-1",
            &data,
            "request-user-input",
            SessionState::Thinking,
            false,
        );

        assert!(matches!(session.state, SessionState::Thinking));
        assert!(!session.task_complete);
        assert!(session.requires_attention);
        assert_eq!(session.model_name, "gpt-test");
        assert_eq!(session.cwd, "/tmp/project");
        assert_eq!(session.pending_requests.len(), 0);

        session.clear_attention_reason("request-user-input");

        assert!(!session.requires_attention);
    }

    #[test]
    fn nonblocking_attention_tracks_independent_reasons() {
        let mut session = SessionObject::new("codex");
        let data = json!({
            "session_id": "session-1",
            "cwd": "/tmp/project",
            "model": "gpt-test"
        });

        apply_nonblocking_attention(
            &mut session,
            "codex",
            "session-1",
            &data,
            "request-user-input",
            SessionState::Thinking,
            false,
        );
        apply_nonblocking_attention(
            &mut session,
            "codex",
            "session-1",
            &data,
            "plan-mode-prompt",
            SessionState::Idle,
            true,
        );

        session.clear_attention_reason("request-user-input");
        assert!(session.requires_attention);

        session.clear_attention_reason("plan-mode-prompt");
        assert!(!session.requires_attention);
    }

    #[test]
    fn plan_mode_prompt_attention_marks_turn_complete_idle() {
        let mut session = SessionObject::new("codex");
        let data = json!({
            "session_id": "session-1",
            "cwd": "/tmp/project",
            "model": "gpt-test"
        });

        apply_nonblocking_attention(
            &mut session,
            "codex",
            "session-1",
            &data,
            "plan-mode-prompt",
            SessionState::Idle,
            true,
        );

        assert!(matches!(session.state, SessionState::Idle));
        assert!(session.task_complete);
        assert!(session.requires_attention);
    }
}
