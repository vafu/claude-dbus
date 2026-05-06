use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};

use crate::dbus::SessionObject;

static CODEX_SESSION_FILE_CACHE: OnceLock<StdMutex<HashMap<String, PathBuf>>> = OnceLock::new();

pub(super) fn apply_usage_limits(
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

pub(super) fn context_pct(data: &serde_json::Value) -> Option<f64> {
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

pub(super) fn codex_context_pct(token_count_payload: &serde_json::Value) -> Option<f64> {
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
    let cache = CODEX_SESSION_FILE_CACHE.get_or_init(|| StdMutex::new(HashMap::new()));
    if let Ok(mut cache) = cache.lock() {
        if let Some(path) = cache.get(session_id) {
            if path.exists() {
                return Some(path.clone());
            }
            cache.remove(session_id);
        }
    }

    let home = std::env::var_os("HOME")?;
    let sessions_dir = Path::new(&home).join(".codex/sessions");
    let mut matches = Vec::new();
    collect_matching_codex_sessions(&sessions_dir, session_id, &mut matches);
    let path = matches.into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    })?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(session_id.to_string(), path.clone());
    }
    Some(path)
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
