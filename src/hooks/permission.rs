use agent_dbus::agent::is_gemini_agent;
use std::path::PathBuf;

const WRITE_DETAIL_PREVIEW_CHARS: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PermissionDetail {
    pub kind: String,
    pub text: String,
}

pub(super) fn build_permission_prompt(data: &serde_json::Value) -> String {
    let tool_name = data["tool_name"].as_str().unwrap_or("unknown tool");
    let input_desc = if let Some(desc) = data["tool_input"]["description"].as_str() {
        desc.to_string()
    } else if is_claude_edit(data) {
        format_file_action("Edit", data)
    } else if is_claude_write(data) {
        format_file_action("Write", data)
    } else if is_apply_patch(data) {
        "apply patch".to_string()
    } else if let Some(cmd) = data["tool_input"]["command"].as_str() {
        first_line_or_summary(cmd)
    } else if let Some(path) = data["tool_input"]["file_path"].as_str() {
        format!("`{}`", path)
    } else {
        serde_json::to_string(&data["tool_input"]).unwrap_or_default()
    };
    format!("Allow {}?\n{}", tool_name, input_desc)
}

pub(super) fn build_permission_detail(data: &serde_json::Value) -> PermissionDetail {
    if let Some(command) = apply_patch_command(data) {
        return PermissionDetail {
            kind: "diff".to_string(),
            text: command.to_string(),
        };
    }

    if is_claude_edit(data) {
        return PermissionDetail {
            kind: "diff".to_string(),
            text: claude_edit_diff(data),
        };
    }

    if is_claude_write(data) {
        return PermissionDetail {
            kind: "text".to_string(),
            text: claude_write_detail(data),
        };
    }

    if let Some(command) = data["tool_input"]["command"].as_str() {
        return PermissionDetail {
            kind: "command".to_string(),
            text: command.to_string(),
        };
    }

    let input = &data["tool_input"];
    if !input.is_null() {
        return PermissionDetail {
            kind: "json".to_string(),
            text: serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string()),
        };
    }

    PermissionDetail {
        kind: String::new(),
        text: String::new(),
    }
}

fn is_apply_patch(data: &serde_json::Value) -> bool {
    apply_patch_command(data).is_some()
}

fn apply_patch_command(data: &serde_json::Value) -> Option<&str> {
    let command = data["tool_input"]["command"].as_str()?;
    let tool_name = data["tool_name"].as_str().unwrap_or("");
    if tool_name == "apply_patch" || command.trim_start().starts_with("*** Begin Patch") {
        Some(command)
    } else {
        None
    }
}

fn is_claude_edit(data: &serde_json::Value) -> bool {
    data["tool_name"].as_str() == Some("Edit")
        && data["tool_input"]["old_string"].as_str().is_some()
        && data["tool_input"]["new_string"].as_str().is_some()
}

fn is_claude_write(data: &serde_json::Value) -> bool {
    data["tool_name"].as_str() == Some("Write") && data["tool_input"]["content"].as_str().is_some()
}

fn format_file_action(action: &str, data: &serde_json::Value) -> String {
    data["tool_input"]["file_path"]
        .as_str()
        .map(|path| format!("{action} `{path}`"))
        .unwrap_or_else(|| action.to_string())
}

fn first_line_or_summary(text: &str) -> String {
    let mut lines = text.lines();
    let first_line = lines.next().unwrap_or(text);
    if lines.next().is_some() {
        format!("`{first_line}` ...")
    } else {
        format!("`{first_line}`")
    }
}

fn claude_edit_diff(data: &serde_json::Value) -> String {
    let path = data["tool_input"]["file_path"]
        .as_str()
        .unwrap_or("unknown");
    let old_string = data["tool_input"]["old_string"].as_str().unwrap_or("");
    let new_string = data["tool_input"]["new_string"].as_str().unwrap_or("");
    let mut diff = format!("--- {path}\n+++ {path}\n@@\n");
    for line in old_string.lines() {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in new_string.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

fn claude_write_detail(data: &serde_json::Value) -> String {
    let path = data["tool_input"]["file_path"]
        .as_str()
        .unwrap_or("unknown");
    let content = data["tool_input"]["content"].as_str().unwrap_or("");
    let mut detail = format!("File: {path}\n\n");
    detail.push_str(&truncate_preview(content, WRITE_DETAIL_PREVIEW_CHARS));
    detail
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_none() {
        return preview;
    }
    preview.push_str("\n\n... truncated ...");
    preview
}

pub(super) fn build_permission_options(data: &serde_json::Value) -> Vec<String> {
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
    if always_allow_options.is_empty() && codex_prefix_rule(data).is_some() {
        always_allow_options.push("Always allow".to_string());
    }
    options.append(&mut always_allow_options);
    options.push("Deny".to_string());
    options
}

pub(super) fn build_permission_option_descriptions(
    data: &serde_json::Value,
    options: &[String],
) -> Vec<String> {
    options
        .iter()
        .map(|option| permission_option_description(data, option))
        .collect()
}

fn is_codex_permission_request(data: &serde_json::Value) -> bool {
    data["hook_event_name"].as_str() == Some("PermissionRequest")
        && data["transcript_path"].as_str().is_some()
        && data["permission_mode"].as_str().is_some()
}

pub(super) fn should_defer_codex_permission_to_auto_review(
    agent_name: &str,
    data: &serde_json::Value,
) -> bool {
    if agent_name != "codex" || !is_codex_permission_request(data) {
        return false;
    }
    if codex_permission_request_is_auto_review_fallback(data) {
        return false;
    }

    if let Some(reviewer) = codex_payload_approval_reviewer(data) {
        return is_codex_auto_approval_reviewer(reviewer);
    }

    codex_config_approval_reviewer()
        .as_deref()
        .is_some_and(is_codex_auto_approval_reviewer)
}

fn codex_permission_request_is_auto_review_fallback(data: &serde_json::Value) -> bool {
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
    .any(is_codex_auto_approval_reviewer)
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

fn codex_payload_approval_reviewer(data: &serde_json::Value) -> Option<&str> {
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

fn is_codex_auto_approval_reviewer(value: &str) -> bool {
    matches!(
        normalize_permission_value(value).as_str(),
        "auto_review" | "autoreview" | "guardian_subagent" | "guardiansubagent" | "guardian"
    )
}

fn normalize_permission_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn codex_config_approval_reviewer() -> Option<String> {
    let path = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?
        .join("config.toml");
    let config = std::fs::read_to_string(path).ok()?;
    parse_codex_config_approval_reviewer(&config)
}

fn parse_codex_config_approval_reviewer(config: &str) -> Option<String> {
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

fn permission_suggestion_label(suggestion: &serde_json::Value) -> String {
    let dest = suggestion["destination"].as_str().unwrap_or("");
    if dest.is_empty() {
        "Always allow".to_string()
    } else {
        format!("Always allow ({dest})")
    }
}

fn permission_option_description(data: &serde_json::Value, option: &str) -> String {
    let normalized = option.trim().to_ascii_lowercase();
    if normalized == "allow" || normalized.starts_with("allow ") {
        return "Allow only this request.".to_string();
    }
    if normalized == "deny" || normalized.starts_with("deny ") {
        return "Deny this request.".to_string();
    }
    if !is_always_allow_answer(option) {
        return String::new();
    }

    if let Some(prefix_rule) = codex_prefix_rule(data) {
        return format!(
            "Persistently allow commands starting with: {}",
            format_rule_values(prefix_rule)
        );
    }

    permission_suggestion_for_answer(data, option)
        .as_ref()
        .map(permission_suggestion_description)
        .unwrap_or_else(|| "Persistently allow matching future requests.".to_string())
}

fn permission_suggestion_description(suggestion: &serde_json::Value) -> String {
    let rules = suggestion["rules"]
        .as_array()
        .map(|rules| {
            rules
                .iter()
                .filter_map(permission_rule_description)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let destination = suggestion["destination"].as_str().unwrap_or("");
    let mut description = if rules.is_empty() {
        "Persistently allow matching future requests.".to_string()
    } else {
        format!("Persistently allow: {}", rules.join("; "))
    };
    if !destination.is_empty() {
        description.push_str(&format!(" Destination: {destination}."));
    }
    description
}

fn permission_rule_description(rule: &serde_json::Value) -> Option<String> {
    let tool_name = rule["toolName"]
        .as_str()
        .or_else(|| rule["tool_name"].as_str());
    let rule_content = rule["ruleContent"]
        .as_str()
        .or_else(|| rule["rule_content"].as_str());

    match (tool_name, rule_content) {
        (Some(tool_name), Some(rule_content)) => Some(format!("{tool_name} {rule_content}")),
        (Some(tool_name), None) => Some(tool_name.to_string()),
        (None, Some(rule_content)) => Some(rule_content.to_string()),
        (None, None) => None,
    }
}

fn format_rule_values(values: &[serde_json::Value]) -> String {
    values
        .iter()
        .map(format_rule_value)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_rule_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

pub(super) fn build_elicitation_options(data: &serde_json::Value) -> Vec<String> {
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

pub(super) fn permission_response(
    agent_name: &str,
    data: &serde_json::Value,
    answer: &str,
) -> Option<String> {
    let answer = answer.trim();
    if is_gemini_agent(agent_name) {
        return gemini_permission_response(answer);
    }

    if is_always_allow_answer(answer) {
        if let Some(prefix_rule) = codex_prefix_rule(data) {
            Some(permission_allow_response_with_exec_policy_amendment(
                prefix_rule.clone(),
            ))
        } else {
            Some(permission_allow_response(
                permission_suggestion_for_answer(data, answer)
                    .into_iter()
                    .collect(),
            ))
        }
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

fn gemini_permission_response(answer: &str) -> Option<String> {
    if is_always_allow_answer(answer) || is_allow_answer(answer) {
        return Some(serde_json::json!({ "decision": "allow" }).to_string());
    }
    if answer.eq_ignore_ascii_case("deny") || answer.starts_with("Deny") {
        return Some(
            serde_json::json!({
                "decision": "deny",
                "reason": "User denied via popup"
            })
            .to_string(),
        );
    }
    None
}

fn codex_prefix_rule(data: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    if !is_codex_permission_request(data) {
        return None;
    }
    data["prefix_rule"].as_array()
}

fn is_allow_answer(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("allow") || answer.starts_with("Allow ")
}

fn is_always_allow_answer(answer: &str) -> bool {
    let normalized = answer.to_ascii_lowercase();
    normalized == "always allow" || normalized.starts_with("always allow ")
}

fn permission_suggestion_for_answer(
    data: &serde_json::Value,
    answer: &str,
) -> Option<serde_json::Value> {
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
        .cloned()
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

fn permission_allow_response_with_exec_policy_amendment(
    exec_policy_amendment: Vec<serde_json::Value>,
) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {
                "behavior": "allow",
                "execPolicyAmendment": exec_policy_amendment
            }
        }
    })
    .to_string()
}
