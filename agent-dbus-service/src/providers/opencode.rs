use agent_dbus_core::agent::is_opencode_agent;

use super::codex::subagent::SubagentInfo;

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
pub(crate) fn opencode_subagent_info(data: &serde_json::Value) -> Option<SubagentInfo> {
    let parent_session_id = json_string_at(data, &["parent_session_id"])
        .or_else(|| json_string_at(data, &["parentID"]))
        .or_else(|| json_string_at(data, &["parentId"]))
        .or_else(|| json_string_at(data, &["parent_id"]))
        .or_else(|| json_string_at(data, &["info", "parentID"]))
        .or_else(|| json_string_at(data, &["payload", "parent_session_id"]))
        .unwrap_or_default();

    if parent_session_id.is_empty() {
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
        let from_flat = opencode_subagent_info(&json!({ "parent_session_id": "ses_parent" }));
        assert_eq!(
            from_flat.map(|info| info.parent_session_id),
            Some("ses_parent".to_string())
        );

        let from_info = opencode_subagent_info(&json!({
            "info": { "parentID": "ses_parent" }
        }));
        assert_eq!(
            from_info.map(|info| info.parent_session_id),
            Some("ses_parent".to_string())
        );

        assert!(opencode_subagent_info(&json!({})).is_none());
        assert!(opencode_subagent_info(&json!({ "parentID": "" })).is_none());
    }
}
