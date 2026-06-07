use crate::dbus::SessionObject;
use crate::providers::codex::metrics::codex_session_metrics;

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
