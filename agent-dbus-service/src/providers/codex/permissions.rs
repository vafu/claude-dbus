use std::path::PathBuf;

pub(crate) fn is_codex_permission_request(data: &serde_json::Value) -> bool {
    data["hook_event_name"].as_str() == Some("PermissionRequest")
        && data["transcript_path"].as_str().is_some()
        && data["permission_mode"].as_str().is_some()
}

pub(crate) fn should_defer_permission_to_auto_review(
    agent_name: &str,
    data: &serde_json::Value,
) -> bool {
    if agent_name != "codex" || !is_codex_permission_request(data) {
        return false;
    }
    if permission_request_is_auto_review_fallback(data) {
        return false;
    }

    if let Some(reviewer) = payload_approval_reviewer(data) {
        return is_auto_approval_reviewer(reviewer);
    }

    config_approval_reviewer()
        .as_deref()
        .is_some_and(is_auto_approval_reviewer)
}

pub(crate) fn prefix_rule(data: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    if !is_codex_permission_request(data) {
        return None;
    }
    data["prefix_rule"].as_array()
}

fn permission_request_is_auto_review_fallback(data: &serde_json::Value) -> bool {
    [
        &data["auto_review_denied"],
        &data["guardian_denied"],
        &data["auto_approval_declined"],
        &data["approval_review"]["denied"],
        &data["auto_review"]["denied"],
        &data["guardian_assessment"]["denied"],
    ]
    .iter()
    .any(|value| value.as_bool() == Some(true))
        || [
            &data["approval_review"]["status"],
            &data["approval_review"]["decision"],
            &data["auto_review"]["status"],
            &data["auto_review"]["decision"],
            &data["guardian_assessment"]["status"],
            &data["guardian_assessment"]["decision"],
            &data["guardian_assessment"]["user_authorization"],
            &data["guardian_assessment"]["action"],
            &data["review_status"],
            &data["review_decision"],
        ]
        .iter()
        .filter_map(|value| value.as_str())
        .any(is_auto_review_decline_value)
        || decision_source_is_auto_reviewer(data)
            && [
                &data["decision"],
                &data["status"],
                &data["user_authorization"],
                &data["approval_decision"],
            ]
            .iter()
            .filter_map(|value| value.as_str())
            .any(is_auto_review_decline_value)
}

fn decision_source_is_auto_reviewer(data: &serde_json::Value) -> bool {
    [
        &data["decision_source"],
        &data["approval_review"]["decision_source"],
        &data["auto_review"]["decision_source"],
        &data["guardian_assessment"]["decision_source"],
    ]
    .iter()
    .filter_map(|value| value.as_str())
    .any(is_auto_approval_reviewer)
}

fn is_auto_review_decline_value(value: &str) -> bool {
    matches!(
        normalize_permission_value(value).as_str(),
        "denied"
            | "deny"
            | "declined"
            | "decline"
            | "rejected"
            | "reject"
            | "aborted"
            | "abort"
            | "blocked"
            | "block"
    )
}

fn payload_approval_reviewer(data: &serde_json::Value) -> Option<&str> {
    [
        &data["approvals_reviewer"],
        &data["approval_reviewer"],
        &data["approval_review_mode"],
        &data["reviewer"],
        &data["approval"]["reviewer"],
        &data["approval"]["reviewer_mode"],
        &data["approval_review"]["reviewer"],
        &data["approval_review"]["mode"],
        &data["auto_review"]["reviewer"],
        &data["guardian_assessment"]["reviewer"],
    ]
    .iter()
    .find_map(|value| value.as_str())
}

fn is_auto_approval_reviewer(value: &str) -> bool {
    matches!(
        normalize_permission_value(value).as_str(),
        "auto_review" | "autoreview" | "guardian_subagent" | "guardiansubagent" | "guardian"
    )
}

fn normalize_permission_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn config_approval_reviewer() -> Option<String> {
    let path = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?
        .join("config.toml");
    let config = std::fs::read_to_string(path).ok()?;
    parse_config_approval_reviewer(&config)
}

fn parse_config_approval_reviewer(config: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let line = line.split_once('#').map_or(line, |(line, _)| line).trim();
        let (key, value) = line.split_once('=')?;
        (key.trim() == "approvals_reviewer")
            .then(|| {
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string()
            })
            .filter(|value| !value.is_empty())
    })
}
