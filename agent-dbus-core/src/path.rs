use zbus::zvariant::ObjectPath;

use crate::constants::ROOT_PATH;

pub fn safe_path_segment(value: &str) -> String {
    let mut safe = String::new();
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            safe.push(c);
        } else {
            safe.push_str(&format!("_{:x}_", c as u32));
        }
    }
    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

pub fn session_path(agent_name: &str, session_id: &str) -> ObjectPath<'static> {
    let safe_agent = safe_path_segment(agent_name);
    let safe_id = safe_path_segment(session_id);
    ObjectPath::try_from(format!("{ROOT_PATH}/sessions/{}/{}", safe_agent, safe_id))
        .expect("safe path segments should always produce a valid object path")
}

pub fn agent_session_node_key(agent_name: &str, session_id: &str) -> String {
    format!(
        "{}/{}",
        safe_path_segment(agent_name),
        safe_path_segment(session_id)
    )
}

pub fn session_key(agent_name: &str, session_id: &str) -> String {
    format!("{}:{}", agent_name, session_id)
}
