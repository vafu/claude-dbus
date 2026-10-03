use agent_dbus_core::agent::is_opencode_agent;

use super::codex::subagent::SubagentInfo;
use crate::dbus::SessionObject;
use agent_dbus_core::telemetry::{TokenUsage, UsageReport, reasoning_effort};

fn token_map(value: &serde_json::Value) -> Option<TokenUsage> {
    let object = value.as_object()?;
    let mut result = TokenUsage::new();
    for key in [
        "input",
        "output",
        "cache_read_input",
        "cache_write_input",
        "reasoning_output",
        "total",
    ] {
        if let Some(value) = object.get(key) {
            result.insert(key.to_owned(), value.as_u64()?);
        }
    }
    let total = result.get("input")?.checked_add(*result.get("output")?)?;
    if result.get("total") != Some(&total) {
        return None;
    }
    Some(result)
}

/// The plugin normalizes OpenCode's exclusive buckets into inclusive totals.
/// Metadata-only events never change lifecycle, and snapshots never emit usage.
pub(crate) fn apply_telemetry(
    session: &mut SessionObject,
    data: &serde_json::Value,
) -> Option<UsageReport> {
    if data.get("token_usage").is_none() {
        if let Some(effort) = data.get("reasoning_effort") {
            session.reasoning_effort = reasoning_effort(effort.as_str());
        }
        if let Some(model) = data["model"]
            .as_str()
            .filter(|m| !m.is_empty() && *m != "unknown")
        {
            session.model_name = model.to_owned();
        }
    }
    if session.token_usage.is_empty() && session.usage_revision == 0 {
        if let Some(baseline) = token_map(&data["usage_baseline"]) {
            session.token_usage = baseline;
        }
    }
    let event_id = data["usage_event_id"].as_str().filter(|s| !s.is_empty())?;
    if session.opencode_usage_events.contains(event_id) {
        return None;
    }
    let delta = token_map(&data["token_usage"])?;
    let mut next = session.token_usage.clone();
    for (key, value) in &delta {
        next.insert(
            key.clone(),
            next.get(key).copied().unwrap_or(0).checked_add(*value)?,
        );
    }
    // The plugin's cumulative snapshot recovers totals after a bridge restart;
    // the live signal still contains only this unique step's consumption.
    if let Some(cumulative) = token_map(&data["cumulative_token_usage"]) {
        if cumulative
            .iter()
            .any(|(k, v)| session.token_usage.get(k).is_some_and(|old| v < old))
        {
            session.usage_epoch += 1;
        }
        next = cumulative;
    }
    session.opencode_usage_events.insert(event_id.to_owned());
    session.opencode_usage_order.push_back(event_id.to_owned());
    if session.opencode_usage_order.len() > 4096 {
        if let Some(old) = session.opencode_usage_order.pop_front() {
            session.opencode_usage_events.remove(&old);
        }
    }
    session.last_token_usage = delta.clone();
    session.token_usage = next.clone();
    session.usage_revision += 1;
    Some(UsageReport {
        epoch: session.usage_epoch,
        revision: session.usage_revision,
        timestamp: data["usage_timestamp"].as_str().unwrap_or("").to_owned(),
        turn_id: data["turn_id"].as_str().unwrap_or("").to_owned(),
        model: data["model"].as_str().unwrap_or("unknown").to_owned(),
        reasoning_effort: reasoning_effort(data["reasoning_effort"].as_str()),
        delta,
        totals: next,
    })
}

/// Maps a shell-UI answer onto the OpenCode permission reply shape.
///
/// The OpenCode bridge plugin (`opencode-plugin/agent-dbus.js`) parses this
/// JSON from `agent-hook` stdout and POSTs it to the live OpenCode listener
/// (see anomalyco/opencode#28037 for why the plugin cannot use the in-process
/// SDK client for replies). Valid replies are `once`, `always`, and `reject`.
pub(crate) fn permission_response(agent_name: &str, answer: &str) -> Option<String> {
    if !is_opencode_agent(agent_name) {
        return None;
    }
    let answer = answer.trim();
    if is_always_allow_answer(answer) {
        return Some(r#"{"reply":"always"}"#.to_string());
    }
    if is_allow_answer(answer) {
        return Some(r#"{"reply":"once"}"#.to_string());
    }
    if answer.eq_ignore_ascii_case("deny") || answer.starts_with("Deny") {
        return Some(r#"{"reply":"reject"}"#.to_string());
    }
    None
}

fn is_allow_answer(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("allow") || answer.starts_with("Allow ")
}

fn is_always_allow_answer(answer: &str) -> bool {
    let normalized = answer.to_ascii_lowercase();
    normalized == "always allow" || normalized.starts_with("always allow ")
}

/// Derives subagent metadata from OpenCode hook payloads.
///
/// OpenCode tracks subagents as child sessions: `session.created` carries the
/// parent in `info.parentID`. The plugin flattens that into the hook data,
/// accepting several aliases because the exact field name differs between the
/// event payload and ad-hoc hook data.
pub(crate) fn opencode_subagent_info(
    session_id: &str,
    data: &serde_json::Value,
) -> Option<SubagentInfo> {
    let parent_session_id = json_string_at(data, &["parent_session_id"])
        .or_else(|| json_string_at(data, &["parentID"]))
        .or_else(|| json_string_at(data, &["parentId"]))
        .or_else(|| json_string_at(data, &["parent_id"]))
        .or_else(|| json_string_at(data, &["info", "parentID"]))
        .or_else(|| json_string_at(data, &["payload", "parent_session_id"]))
        .unwrap_or_default();

    if parent_session_id.is_empty() || parent_session_id == session_id {
        return None;
    }

    Some(SubagentInfo {
        parent_session_id,
        nickname: json_string_at(data, &["agent_nickname"])
            .or_else(|| json_string_at(data, &["payload", "agent_nickname"]))
            .unwrap_or_default(),
        role: json_string_at(data, &["agent_role"])
            .or_else(|| json_string_at(data, &["payload", "agent_role"]))
            .unwrap_or_default(),
    })
}

fn json_string_at(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opencode_agent_matches_known_names() {
        assert!(is_opencode_agent("opencode"));
        assert!(is_opencode_agent("opencode-cli"));
        assert!(!is_opencode_agent("codex"));
        assert!(!is_opencode_agent("claude"));
    }

    #[test]
    fn opencode_permission_response_uses_reply_shape() {
        let allow: serde_json::Value =
            serde_json::from_str(&permission_response("opencode", "Allow").unwrap()).unwrap();
        assert_eq!(allow, json!({ "reply": "once" }));

        let always: serde_json::Value =
            serde_json::from_str(&permission_response("opencode-cli", "Always allow").unwrap())
                .unwrap();
        assert_eq!(always, json!({ "reply": "always" }));

        let deny: serde_json::Value =
            serde_json::from_str(&permission_response("opencode", "Deny").unwrap()).unwrap();
        assert_eq!(deny, json!({ "reply": "reject" }));

        assert_eq!(permission_response("opencode", ""), None);
        assert_eq!(permission_response("opencode", "maybe later"), None);
        assert_eq!(permission_response("codex", "Allow"), None);
    }

    #[test]
    fn opencode_subagent_info_accepts_parent_aliases() {
        let from_flat =
            opencode_subagent_info("ses_child", &json!({ "parent_session_id": "ses_parent" }));
        assert_eq!(
            from_flat.map(|info| info.parent_session_id),
            Some("ses_parent".to_string())
        );

        let from_info = opencode_subagent_info(
            "ses_child",
            &json!({
                "info": { "parentID": "ses_parent" }
            }),
        );
        assert_eq!(
            from_info.map(|info| info.parent_session_id),
            Some("ses_parent".to_string())
        );

        assert!(opencode_subagent_info("ses_child", &json!({})).is_none());
        assert!(opencode_subagent_info("ses_child", &json!({ "parentID": "" })).is_none());
    }

    #[test]
    fn opencode_subagent_info_rejects_self_parenting() {
        assert!(
            opencode_subagent_info("ses_1", &json!({ "parent_session_id": "ses_1" })).is_none()
        );
    }

    #[test]
    fn telemetry_baselines_deduplicates_and_preserves_state_and_selected_model() {
        let mut s = SessionObject::default();
        s.state = crate::types::SessionState::Thinking;
        s.model_name = "new-model".to_owned();
        assert!(apply_telemetry(&mut s, &json!({"reasoning_effort":"high", "usage_baseline":{"input":100,"output":20,"total":120}})).is_none());
        let event = json!({"usage_event_id":"step-1","model":"old-model","reasoning_effort":"low", "token_usage":{"input":10,"output":2,"total":12,"reasoning_output":1,"cache_read_input":5}});
        let report = apply_telemetry(&mut s, &event).unwrap();
        assert_eq!(report.model, "old-model");
        assert_eq!(report.reasoning_effort, "low");
        assert_eq!(report.totals["input"], 110);
        assert_eq!(s.reasoning_effort, "high");
        assert_eq!(s.model_name, "new-model");
        assert!(s.state == crate::types::SessionState::Thinking);
        assert!(apply_telemetry(&mut s, &event).is_none());
        assert_eq!(s.usage_revision, 1);
    }

    #[test]
    fn telemetry_rejects_invalid_counts_and_resyncs_cumulative_without_backfill() {
        let mut s = SessionObject::default();
        let bad = json!({"usage_event_id":"a","token_usage":{"input":-1,"output":2,"total":1}});
        assert!(apply_telemetry(&mut s, &bad).is_none());
        assert!(s.token_usage.is_empty());
        let report = apply_telemetry(&mut s,&json!({"usage_event_id":"a", "token_usage":{"input":10,"output":2,"total":12}, "cumulative_token_usage":{"input":1000,"output":200,"total":1200}})).unwrap();
        assert_eq!(report.delta["total"], 12);
        assert_eq!(report.totals["total"], 1200);
        assert!(!report.delta.contains_key("cache_write_input"));
    }
}
