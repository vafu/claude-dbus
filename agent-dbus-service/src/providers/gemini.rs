pub use agent_dbus_core::agent::is_gemini_agent;

pub(crate) fn permission_response(answer: &str) -> Option<String> {
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

fn is_allow_answer(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("allow") || answer.starts_with("Allow ")
}

fn is_always_allow_answer(answer: &str) -> bool {
    let normalized = answer.to_ascii_lowercase();
    normalized == "always allow" || normalized.starts_with("always allow ")
}
