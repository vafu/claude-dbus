pub(super) fn build_permission_prompt(data: &serde_json::Value) -> String {
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

pub(super) fn permission_response(data: &serde_json::Value, answer: &str) -> Option<String> {
    let answer = answer.trim();
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

fn permission_suggestion_for_answer<'a>(
    data: &'a serde_json::Value,
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
