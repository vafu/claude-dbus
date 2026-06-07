use super::artifacts::{codex_session_file, read_file_tail};

const CODEX_METRICS_TAIL_READ_MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct SessionMetrics {
    pub context_pct: Option<f64>,
    pub five_hour: Option<(f64, u64)>,
    pub seven_day: Option<(f64, u64)>,
}

pub(crate) fn codex_session_metrics(session_id: &str) -> Option<SessionMetrics> {
    let path = codex_session_file(session_id)?;
    let contents = read_file_tail(&path, CODEX_METRICS_TAIL_READ_MAX_BYTES)?;

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

pub(crate) fn codex_context_pct(token_count_payload: &serde_json::Value) -> Option<f64> {
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
