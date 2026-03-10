use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

use crate::EndedSessions;
use crate::dbus::{SessionObject, create_session, emit_elicitation, emit_notification, session_path, update_session};
use crate::types::SessionState;

pub async fn handle_hook_connection(
    mut stream: tokio::net::UnixStream,
    conn: zbus::Connection,
    ended: EndedSessions,
) {
    let mut buf = Vec::new();
    if stream.read_to_end(&mut buf).await.is_err() {
        return;
    }
    let raw = match std::str::from_utf8(&buf) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };

    let msg: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            info!("Failed to parse hook message: {}", e);
            return;
        }
    };

    let event = msg["event"].as_str().unwrap_or("").to_string();
    let data = &msg["data"];
    let session_id = data["session_id"].as_str().unwrap_or("unknown").to_string();

    info!(event = %event, session_id = %session_id, "hook received");
    tracing::debug!(data = %data, "hook data");

    match event.as_str() {
        "UpdateState" => {
            if ended.lock().await.remove(&session_id) {
                info!(session_id = %session_id, "skipping UpdateState for ended session");
                return;
            }
            let ctx_pct = data["context_window"]["used_percentage"]
                .as_f64()
                .unwrap_or(0.0);
            let model = data["model"]["display_name"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            let cwd = data["cwd"].as_str().unwrap_or("").to_string();
            let cost_usd = data["cost"]["total_cost_usd"].as_f64().unwrap_or(0.0);
            let _ = update_session(&conn, &session_id, |d| {
                d.context_pct = ctx_pct;
                d.model_name = model;
                d.cwd = cwd;
                d.cost_usd = cost_usd;
                d.requires_attention = false;
                if d.state == SessionState::NoSession {
                    d.state = SessionState::Idle;
                }
            })
            .await;
        }

        "SessionStart" => {
            let _ = create_session(&conn, &session_id).await;
            let _ = update_session(&conn, &session_id, |d| {
                d.state = SessionState::Idle;
            })
            .await;
        }

        "Stop" => {
            let _ = update_session(&conn, &session_id, |d| {
                d.state = SessionState::Idle;
                d.requires_attention = false;
            })
            .await;
        }

        "SessionEnd" => {
            ended.lock().await.insert(session_id.clone());
            let path = session_path(&session_id);
            let _ = conn
                .object_server()
                .remove::<SessionObject, _>(&path)
                .await;
        }

        "TaskCompleted" => {
            let _ = update_session(&conn, &session_id, |d| {
                d.task_complete = true;
            })
            .await;
        }

        "UserPromptSubmit" => {
            let _ = update_session(&conn, &session_id, |d| {
                d.state = SessionState::Thinking;
                d.task_complete = false;
                d.requires_attention = false;
            })
            .await;
        }

        "PreToolUse" => {
            let _ = update_session(&conn, &session_id, |d| {
                d.state = SessionState::ToolUse;
            })
            .await;
        }

        "PostToolUse" => {
            let _ = update_session(&conn, &session_id, |d| {
                d.requires_attention = false;
                d.elicitation_tx = None;
            })
            .await;
        }

        "Notify" => {
            let message = data["message"].as_str().unwrap_or("").to_string();
            let path = session_path(&session_id);
            if let Ok(iface_ref) = conn
                .object_server()
                .interface::<_, SessionObject>(&path)
                .await
            {
                let emitter = iface_ref.signal_emitter();
                let _ = emit_notification(emitter, &message).await;
            }
        }


        "PreCompact" => {
            let _ = update_session(&conn, &session_id, |d| {
                d.state = SessionState::Compacting;
                d.requires_attention = false;
            })
            .await;
        }

        "PermissionRequest" => {
            let response = handle_elicitation_event(
                &conn,
                &session_id,
                build_permission_prompt(data),
                build_permission_options(data),
            )
            .await;
            let decision = if response.starts_with("Allow") {
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
            } else {
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"User denied via popup"}}}"#
            };
            let _ = stream.write_all(decision.as_bytes()).await;
        }

        "Elicitation" => {
            let prompt = data["elicitation"]["message"]
                .as_str()
                .or_else(|| data["message"].as_str())
                .unwrap_or("Claude needs input")
                .to_string();
            let options: Vec<String> = data["elicitation"]["options"]
                .as_array()
                .or_else(|| data["options"].as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            v["value"].as_str().or_else(|| v.as_str()).map(String::from)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let response =
                handle_elicitation_event(&conn, &session_id, prompt, options).await;
            let _ = stream.write_all(response.as_bytes()).await;
        }

        other => {
            info!("Unknown hook event: {}", other);
        }
    }
}

fn build_permission_prompt(data: &serde_json::Value) -> String {
    let tool_name = data["tool_name"].as_str().unwrap_or("unknown tool");
    let input_desc = if let Some(cmd) = data["tool_input"]["command"].as_str() {
        format!("`{}`", cmd.chars().take(120).collect::<String>())
    } else if let Some(path) = data["tool_input"]["file_path"].as_str() {
        format!("`{}`", path)
    } else {
        serde_json::to_string(&data["tool_input"]).unwrap_or_default()
    };
    format!("Allow {}?\n{}", tool_name, input_desc)
}

fn build_permission_options(data: &serde_json::Value) -> Vec<String> {
    let mut options: Vec<String> = data["permission_suggestions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let behavior = s["behavior"].as_str()?;
            let dest = s["destination"].as_str().unwrap_or("");
            if behavior == "allow" {
                Some(format!("Allow ({})", dest))
            } else {
                None
            }
        })
        .collect();
    if options.is_empty() {
        options.push("Allow".to_string());
    }
    options.push("Deny".to_string());
    options
}

async fn handle_elicitation_event(
    conn: &zbus::Connection,
    session_id: &str,
    prompt: String,
    options: Vec<String>,
) -> String {
    use tokio::sync::oneshot;
    info!(session_id = %session_id, prompt = %prompt, ?options, "elicitation");

    let path = session_path(session_id);
    let iface_ref = match conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    {
        Ok(r) => r,
        Err(_) => return String::new(),
    };

    let (tx, rx) = oneshot::channel();
    {
        let mut iface = iface_ref.get_mut().await;
        iface.requires_attention = true;
        iface.elicitation_tx = Some(tx);
    }

    let emitter = iface_ref.signal_emitter();
    {
        let iface = iface_ref.get().await;
        let _ = iface.requires_attention_changed(emitter).await;
    }

    let option_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
    let _ = emit_elicitation(emitter, &prompt, &option_refs).await;

    let answer = rx.await.unwrap_or_default();
    info!(session_id = %session_id, answer = %answer, "elicitation answered");

    {
        let mut iface = iface_ref.get_mut().await;
        iface.requires_attention = false;
    }
    {
        let iface = iface_ref.get().await;
        let _ = iface.requires_attention_changed(emitter).await;
    }

    answer
}
