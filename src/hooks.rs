use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;
use zbus::object_server::SignalContext;

use crate::dbus::{ClaudeStatus, restore_after_attention, set_state};
use crate::types::{ClaudeData, ElicitationTxs, Sessions};

pub async fn handle_hook_connection(
    mut stream: tokio::net::UnixStream,
    sessions: Sessions,
    elicitation_txs: ElicitationTxs,
    conn: zbus::Connection,
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

    let ctxt = match SignalContext::new(&conn, "/com/anthropic/ClaudeCode") {
        Ok(c) => c,
        Err(e) => {
            info!("Signal context error: {}", e);
            return;
        }
    };

    match event.as_str() {
        "UpdateState" => {
            let mut map = sessions.lock().await;
            let entry = map
                .entry(session_id.clone())
                .or_insert_with(ClaudeData::default);
            entry.context_used_pct = data["context_window"]["used_percentage"]
                .as_f64()
                .unwrap_or(0.0);
            entry.model_name = data["model"]["display_name"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            if entry.state == "no-session" {
                entry.state = "thinking".to_string();
                entry.pre_attention_state = "thinking".to_string();
            }
            let (state, ctx_pct, model_name) = (
                entry.state.clone(),
                entry.context_used_pct,
                entry.model_name.clone(),
            );
            drop(map);
            let _ = ClaudeStatus::status_changed(&ctxt, &session_id, &state, ctx_pct, &model_name)
                .await;
        }

        "Stop" => {
            let _ = set_state(&sessions, &ctxt, &session_id, "idle").await;
        }

        "SessionStart" => {
            let _ = set_state(&sessions, &ctxt, &session_id, "thinking").await;
        }

        "SessionEnd" => {
            sessions.lock().await.remove(&session_id);
            elicitation_txs.lock().await.remove(&session_id);
            let _ = ClaudeStatus::session_removed(&ctxt, &session_id).await;
        }

        "TaskCompleted" => {
            let _ = set_state(&sessions, &ctxt, &session_id, "attention").await;
        }

        "UserPromptSubmit" => {
            let _ = set_state(&sessions, &ctxt, &session_id, "thinking").await;
        }

        "PostToolUse" => {
            elicitation_txs.lock().await.remove(&session_id);
            let _ = restore_after_attention(&sessions, &ctxt, &session_id).await;
        }

        "Notify" => {
            let message = data["message"].as_str().unwrap_or("");
            if !message.trim().is_empty() {
                let _ = set_state(&sessions, &ctxt, &session_id, "attention").await;
            }
        }

        "PreCompact" => {
            let _ = set_state(&sessions, &ctxt, &session_id, "compacting").await;
        }

        "PermissionRequest" => {
            let response = handle_elicitation_event(
                data,
                &session_id,
                build_permission_prompt(data),
                build_permission_options(data),
                &sessions,
                &elicitation_txs,
                &ctxt,
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
            let response = handle_elicitation_event(
                data,
                &session_id,
                prompt,
                options,
                &sessions,
                &elicitation_txs,
                &ctxt,
            )
            .await;
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
        .unwrap_or(&vec![])
        .iter()
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
    _data: &serde_json::Value,
    session_id: &str,
    prompt: String,
    options: Vec<String>,
    sessions: &Sessions,
    elicitation_txs: &ElicitationTxs,
    ctxt: &SignalContext<'_>,
) -> String {
    use tokio::sync::oneshot;
    info!(session_id = %session_id, prompt = %prompt, ?options, "elicitation");

    let (tx, rx) = oneshot::channel();
    elicitation_txs
        .lock()
        .await
        .insert(session_id.to_string(), tx);

    let option_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
    if ClaudeStatus::elicitation_requested(ctxt, session_id, &prompt, &option_refs)
        .await
        .is_err()
    {
        return String::new();
    }

    {
        let mut map = sessions.lock().await;
        let data = map
            .entry(session_id.to_string())
            .or_insert_with(ClaudeData::default);
        data.pre_attention_state = data.state.clone();
        data.state = "attention".to_string();
        let (ctx_pct, model_name) = (data.context_used_pct, data.model_name.clone());
        drop(map);
        let _ =
            ClaudeStatus::status_changed(ctxt, session_id, "attention", ctx_pct, &model_name).await;
    }

    let answer = rx.await.unwrap_or_default();
    info!(session_id = %session_id, answer = %answer, "elicitation answered");
    answer
}
