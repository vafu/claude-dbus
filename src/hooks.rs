use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, sleep};
use tracing::info;

use crate::dbus::{
    SessionObject, create_session, emit_elicitation, emit_notification, session_path,
    update_session,
};
use crate::types::SessionState;
use crate::{CodexSessionParents, ElicitationLocks, EndedSessions};

pub async fn handle_hook_connection(
    mut stream: tokio::net::UnixStream,
    conn: zbus::Connection,
    ended: EndedSessions,
    elicitation_locks: ElicitationLocks,
    codex_session_parents: CodexSessionParents,
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

    let data = &msg["data"];
    let event = msg["event"].as_str().unwrap_or("").to_string();
    let agent_name = msg["agent"]
        .as_str()
        .or_else(|| msg["agent_name"].as_str())
        .or_else(|| data["agent_name"].as_str())
        .unwrap_or("agent")
        .to_string();
    let session_id = data["session_id"].as_str().unwrap_or("unknown").to_string();
    info!(agent = %agent_name, event = %event, session_id = %session_id, "hook received");
    tracing::debug!(data = %data, "hook data");

    maybe_watch_codex_parent(
        &conn,
        &ended,
        &codex_session_parents,
        &agent_name,
        &session_id,
        msg["parent_pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok()),
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
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
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
            .await;
        }

        "SessionStart" => {
            let _ = create_session(&conn, &agent_name, &session_id).await;
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.state = SessionState::Idle;
                d.model_name = model_name(data);
                d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                apply_usage_limits(d, &agent_name, &session_id, data);
                d.task_complete = false;
                clear_pending_if_not_waiting(d);
            })
            .await;
        }

        "Stop" => {
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.state = SessionState::Idle;
                d.task_complete = true;
                clear_pending_if_not_waiting(d);
                d.model_name = model_name(data);
                d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                apply_usage_limits(d, &agent_name, &session_id, data);
            })
            .await;
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
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.task_complete = true;
            })
            .await;
        }

        "UserPromptSubmit" => {
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.state = SessionState::Thinking;
                d.task_complete = false;
                clear_pending_if_not_waiting(d);
                d.model_name = model_name(data);
                d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                apply_usage_limits(d, &agent_name, &session_id, data);
            })
            .await;
        }

        "PreToolUse" => {
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.state = SessionState::ToolUse;
                d.model_name = model_name(data);
                d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                apply_usage_limits(d, &agent_name, &session_id, data);
            })
            .await;
        }

        "PostToolUse" => {
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.state = SessionState::Thinking;
                clear_pending_if_not_waiting(d);
                d.model_name = model_name(data);
                d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                apply_usage_limits(d, &agent_name, &session_id, data);
            })
            .await;
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
                let _ = emit_notification(emitter, &message).await;
            }
        }

        "PreCompact" => {
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.state = SessionState::Compacting;
                clear_pending_if_not_waiting(d);
            })
            .await;
        }

        "PermissionRequest" => {
            let _ = update_session(&conn, &agent_name, &session_id, |d| {
                d.model_name = model_name(data);
                d.cwd = data["cwd"].as_str().unwrap_or("").to_string();
                apply_usage_limits(d, &agent_name, &session_id, data);
            })
            .await;
            let response = handle_elicitation_event(
                &conn,
                &elicitation_locks,
                &agent_name,
                &session_id,
                build_permission_prompt(data),
                build_permission_options(data),
            )
            .await;
            if let Some(decision) = permission_response(data, &response) {
                let _ = stream.write_all(decision.as_bytes()).await;
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
            let response = handle_elicitation_event(
                &conn,
                &elicitation_locks,
                &agent_name,
                &session_id,
                prompt,
                options,
            )
            .await;
            let _ = stream.write_all(response.as_bytes()).await;
        }

        other => {
            info!("Unknown hook event: {}", other);
        }
    }
}

async fn maybe_watch_codex_parent(
    conn: &zbus::Connection,
    ended: &EndedSessions,
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
    drop(parents);

    let conn = conn.clone();
    let ended = ended.clone();
    let codex_session_parents = codex_session_parents.clone();
    let agent_name = agent_name.to_string();
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(5)).await;
            if process_exists(parent_pid) {
                continue;
            }

            let still_current =
                codex_session_parents.lock().await.get(&key).copied() == Some(parent_pid);
            if still_current {
                remove_session(
                    &conn,
                    &ended,
                    &codex_session_parents,
                    &agent_name,
                    &session_id,
                )
                .await;
                info!(
                    session_id = %session_id,
                    parent_pid,
                    "removed codex session after parent process exited"
                );
            }
            break;
        }
    });
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
    let _ = conn.object_server().remove::<SessionObject, _>(&path).await;
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn build_permission_prompt(data: &serde_json::Value) -> String {
    let tool_name = data["tool_name"].as_str().unwrap_or("unknown tool");
    let input_desc = if let Some(desc) = data["tool_input"]["description"].as_str() {
        desc.to_string()
    } else if let Some(cmd) = data["tool_input"]["command"].as_str() {
        format!("`{}`", cmd.chars().take(120).collect::<String>())
    } else if let Some(path) = data["tool_input"]["file_path"].as_str() {
        format!("`{}`", path)
    } else {
        serde_json::to_string(&data["tool_input"]).unwrap_or_default()
    };
    format!("Allow {}?\n{}", tool_name, input_desc)
}

fn build_permission_options(data: &serde_json::Value) -> Vec<String> {
    let mut options = vec!["Allow".to_string()];
    let mut always_allow_options: Vec<String> = data["permission_suggestions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            if s["behavior"].as_str()? == "allow" {
                Some(permission_suggestion_label(s))
            } else {
                None
            }
        })
        .collect();
    options.append(&mut always_allow_options);
    options.push("Deny".to_string());
    options
}

fn permission_suggestion_label(suggestion: &serde_json::Value) -> String {
    let dest = suggestion["destination"].as_str().unwrap_or("");
    if dest.is_empty() {
        "Always allow".to_string()
    } else {
        format!("Always allow ({dest})")
    }
}

fn build_elicitation_options(data: &serde_json::Value) -> Vec<String> {
    let mut options: Vec<String> = data["elicitation"]["options"]
        .as_array()
        .or_else(|| data["options"].as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v["value"].as_str().or_else(|| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if supports_always_allow(data)
        && has_allow_option(&options)
        && !has_always_allow_option(&options)
    {
        let insert_at = options
            .iter()
            .position(|option| is_decline_option(option))
            .unwrap_or(options.len());
        options.insert(insert_at, "Always allow".to_string());
    }

    options
}

fn supports_always_allow(data: &serde_json::Value) -> bool {
    [
        &data["_meta"]["persist"],
        &data["elicitation"]["_meta"]["persist"],
        &data["meta"]["persist"],
        &data["elicitation"]["meta"]["persist"],
    ]
    .iter()
    .any(|persist| persist_value_includes(persist, "always"))
}

fn persist_value_includes(value: &serde_json::Value, needle: &str) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(needle))
        || value.as_array().is_some_and(|arr| {
            arr.iter()
                .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(needle)))
        })
}

fn has_allow_option(options: &[String]) -> bool {
    options.iter().any(|option| {
        let normalized = option.trim().to_ascii_lowercase();
        normalized == "allow" || normalized == "accept"
    })
}

fn has_always_allow_option(options: &[String]) -> bool {
    options
        .iter()
        .any(|option| option.trim().eq_ignore_ascii_case("always allow"))
}

fn is_decline_option(option: &str) -> bool {
    let normalized = option.trim().to_ascii_lowercase();
    normalized == "deny" || normalized == "decline" || normalized == "cancel"
}

fn permission_response(data: &serde_json::Value, answer: &str) -> Option<String> {
    let answer = answer.trim();
    if is_always_allow_answer(answer) {
        let updated_permissions = permission_suggestion_for_answer(data, answer)
            .map(|suggestion| vec![suggestion.clone()])
            .unwrap_or_default();
        Some(permission_allow_response(updated_permissions))
    } else if is_allow_answer(answer) {
        Some(permission_allow_response(Vec::new()))
    } else if answer.eq_ignore_ascii_case("deny") || answer.starts_with("Deny") {
        Some(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"User denied via popup"}}}"#
                .to_string(),
        )
    } else {
        None
    }
}

fn is_allow_answer(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("allow") || answer.starts_with("Allow ")
}

fn is_always_allow_answer(answer: &str) -> bool {
    let normalized = answer.to_ascii_lowercase();
    normalized == "always allow" || normalized.starts_with("always allow ")
}

fn permission_suggestion_for_answer<'a>(
    data: &'a serde_json::Value,
    answer: &str,
) -> Option<&'a serde_json::Value> {
    let suggestions = data["permission_suggestions"].as_array()?;
    let answer = answer.trim();
    suggestions
        .iter()
        .find(|suggestion| {
            suggestion["behavior"].as_str() == Some("allow")
                && permission_suggestion_label(suggestion).eq_ignore_ascii_case(answer)
        })
        .or_else(|| {
            suggestions
                .iter()
                .find(|suggestion| suggestion["behavior"].as_str() == Some("allow"))
        })
}

fn permission_allow_response(updated_permissions: Vec<serde_json::Value>) -> String {
    let mut decision = serde_json::json!({ "behavior": "allow" });
    if !updated_permissions.is_empty() {
        decision["updatedPermissions"] = serde_json::Value::Array(updated_permissions);
    }

    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision
        }
    })
    .to_string()
}

fn model_name(data: &serde_json::Value) -> String {
    data["model"]["display_name"]
        .as_str()
        .or_else(|| data["model"].as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn session_key(agent_name: &str, session_id: &str) -> String {
    format!("{}:{}", agent_name, session_id)
}

fn apply_usage_limits(
    session: &mut SessionObject,
    agent_name: &str,
    session_id: &str,
    data: &serde_json::Value,
) {
    let fallback = if agent_name == "codex" {
        codex_session_metrics(session_id)
    } else {
        None
    };

    if let Some(pct) = context_pct(data).or_else(|| fallback.as_ref().and_then(|m| m.context_pct)) {
        session.context_pct = pct;
    }

    if let Some((pct, resets_at)) = usage_limit(data, "primary")
        .or_else(|| usage_limit(data, "five_hour"))
        .or_else(|| usage_limit(data, "fiveHour"))
        .or_else(|| usage_limit(data, "5h"))
        .or(fallback.as_ref().and_then(|metrics| metrics.five_hour))
    {
        session.five_hour_usage_pct = pct;
        session.five_hour_resets_at = resets_at;
    }

    if let Some((pct, resets_at)) = usage_limit(data, "secondary")
        .or_else(|| usage_limit(data, "seven_day"))
        .or_else(|| usage_limit(data, "sevenDay"))
        .or_else(|| usage_limit(data, "7d"))
        .or(fallback.as_ref().and_then(|metrics| metrics.seven_day))
    {
        session.seven_day_usage_pct = pct;
        session.seven_day_resets_at = resets_at;
    }
}

fn context_pct(data: &serde_json::Value) -> Option<f64> {
    data["context_window"]["used_percentage"]
        .as_f64()
        .or_else(|| data["context_window"]["used_percent"].as_f64())
        .or_else(|| data["context"]["used_percentage"].as_f64())
        .or_else(|| data["context"]["used_percent"].as_f64())
        .or_else(|| data["context_pct"].as_f64())
}

fn usage_limit(data: &serde_json::Value, key: &str) -> Option<(f64, u64)> {
    let limit = data["rate_limits"][key]
        .as_object()
        .map(|_| &data["rate_limits"][key])
        .or_else(|| data["usage"][key].as_object().map(|_| &data["usage"][key]))
        .or_else(|| {
            data["limits"][key]
                .as_object()
                .map(|_| &data["limits"][key])
        })?;

    let pct = limit["used_percent"]
        .as_f64()
        .or_else(|| limit["usage_percent"].as_f64())
        .or_else(|| limit["used_pct"].as_f64())
        .or_else(|| limit["percent"].as_f64())?;
    let resets_at = limit["resets_at"]
        .as_u64()
        .or_else(|| limit["reset_at"].as_u64())
        .or_else(|| limit["reset_time"].as_u64())
        .unwrap_or(0);

    Some((pct, resets_at))
}

#[derive(Clone, Copy)]
struct SessionMetrics {
    context_pct: Option<f64>,
    five_hour: Option<(f64, u64)>,
    seven_day: Option<(f64, u64)>,
}

fn codex_session_metrics(session_id: &str) -> Option<SessionMetrics> {
    let path = codex_session_file(session_id)?;
    let contents = std::fs::read_to_string(path).ok()?;

    contents.lines().rev().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        let payload = &entry["payload"];
        if entry["type"].as_str()? != "event_msg" || payload["type"].as_str()? != "token_count" {
            return None;
        }

        let data = serde_json::json!({ "rate_limits": payload["rate_limits"].clone() });
        Some(SessionMetrics {
            context_pct: codex_context_pct(payload),
            five_hour: usage_limit(&data, "primary"),
            seven_day: usage_limit(&data, "secondary"),
        })
    })
}

fn codex_context_pct(token_count_payload: &serde_json::Value) -> Option<f64> {
    context_pct(token_count_payload).or_else(|| {
        let window = token_count_payload["info"]["model_context_window"].as_f64()?;
        if window <= 0.0 {
            return None;
        }

        let used = token_count_payload["info"]["last_token_usage"]["total_tokens"]
            .as_f64()
            .or_else(|| token_count_payload["info"]["last_token_usage"]["input_tokens"].as_f64())?;
        Some((used / window) * 100.0)
    })
}

fn codex_session_file(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let sessions_dir = Path::new(&home).join(".codex/sessions");
    let mut matches = Vec::new();
    collect_matching_codex_sessions(&sessions_dir, session_id, &mut matches);
    matches.into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    })
}

fn collect_matching_codex_sessions(dir: &Path, session_id: &str, matches: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            collect_matching_codex_sessions(&path, session_id, matches);
            continue;
        }

        if file_type.is_file()
            && path.extension().is_some_and(|ext| ext == "jsonl")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(session_id))
        {
            matches.push(path);
        }
    }
}

async fn handle_elicitation_event(
    conn: &zbus::Connection,
    elicitation_locks: &ElicitationLocks,
    agent_name: &str,
    session_id: &str,
    prompt: String,
    options: Vec<String>,
) -> String {
    use tokio::sync::oneshot;
    info!(agent = %agent_name, session_id = %session_id, prompt = %prompt, ?options, "elicitation");

    let session_lock = {
        let mut locks = elicitation_locks.lock().await;
        locks
            .entry(session_key(agent_name, session_id))
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _session_guard = session_lock.lock().await;

    let path = session_path(agent_name, session_id);
    let _ = conn
        .object_server()
        .at(&path, SessionObject::new(agent_name))
        .await;
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
        iface.pending_prompt = prompt.clone();
        iface.pending_options = options.clone();
        iface.requires_attention = true;
        iface.elicitation_tx = Some(tx);
    }

    let emitter = iface_ref.signal_emitter();
    {
        let iface = iface_ref.get().await;
        let _ = iface.pending_prompt_changed(emitter).await;
        let _ = iface.pending_options_changed(emitter).await;
        let _ = iface.requires_attention_changed(emitter).await;
    }

    let option_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
    let _ = emit_elicitation(emitter, &prompt, &option_refs).await;

    let answer = match rx.await {
        Ok(answer) => answer,
        Err(_) => {
            info!(agent = %agent_name, session_id = %session_id, "elicitation waiter was dropped before an explicit response");
            String::new()
        }
    };
    info!(agent = %agent_name, session_id = %session_id, answer = %answer, "elicitation answered");

    let mut should_clear = false;
    {
        let mut iface = iface_ref.get_mut().await;
        if iface.pending_prompt == prompt && iface.elicitation_tx.is_none() {
            iface.requires_attention = false;
            iface.pending_prompt.clear();
            iface.pending_options.clear();
            should_clear = true;
        }
    }
    if should_clear {
        let iface = iface_ref.get().await;
        let _ = iface.requires_attention_changed(emitter).await;
        let _ = iface.pending_prompt_changed(emitter).await;
        let _ = iface.pending_options_changed(emitter).await;
    }

    answer
}

fn clear_pending_if_not_waiting(session: &mut SessionObject) {
    if session.elicitation_tx.is_none() {
        session.requires_attention = false;
        session.pending_prompt.clear();
        session.pending_options.clear();
    }
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
}
