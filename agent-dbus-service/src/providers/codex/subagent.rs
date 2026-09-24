use std::io::Read;
use std::path::Path;

use super::artifacts::codex_session_file;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SubagentInfo {
    pub parent_session_id: String,
    pub nickname: String,
    pub role: String,
}

pub(crate) fn codex_subagent_info(
    agent_name: &str,
    session_id: &str,
    data: &serde_json::Value,
) -> Option<SubagentInfo> {
    if agent_name != "codex" {
        return None;
    }

    subagent_info_from_value(data)
        .or_else(|| codex_session_meta_from_hook(session_id, data))
        .or_else(|| {
            codex_session_file(session_id)
                .and_then(|path| codex_session_meta_from_path(session_id, &path))
        })
        .and_then(|meta| {
            if meta.parent_session_id.is_empty() || meta.parent_session_id == session_id {
                None
            } else {
                Some(meta)
            }
        })
}

pub(crate) fn subagent_info_from_value(value: &serde_json::Value) -> Option<SubagentInfo> {
    let thread_source = json_string_at(value, &["thread_source"])
        .or_else(|| json_string_at(value, &["payload", "thread_source"]));
    let explicit_subagent = thread_source.as_deref() == Some("subagent")
        || value
            .pointer("/source/subagent/thread_spawn")
            .is_some_and(serde_json::Value::is_object)
        || value
            .pointer("/payload/source/subagent/thread_spawn")
            .is_some_and(serde_json::Value::is_object);

    let parent_session_id = json_string_at(
        value,
        &["source", "subagent", "thread_spawn", "parent_thread_id"],
    )
    .or_else(|| {
        json_string_at(
            value,
            &[
                "payload",
                "source",
                "subagent",
                "thread_spawn",
                "parent_thread_id",
            ],
        )
    })
    .or_else(|| json_string_at(value, &["thread_spawn", "parent_thread_id"]))
    .or_else(|| json_string_at(value, &["parent_thread_id"]))
    .unwrap_or_default();

    if !explicit_subagent && parent_session_id.is_empty() {
        return None;
    }

    Some(SubagentInfo {
        parent_session_id,
        nickname: json_string_at(value, &["agent_nickname"])
            .or_else(|| json_string_at(value, &["payload", "agent_nickname"]))
            .unwrap_or_default(),
        role: json_string_at(value, &["agent_role"])
            .or_else(|| json_string_at(value, &["payload", "agent_role"]))
            .unwrap_or_default(),
    })
}

fn codex_session_meta_from_hook(
    session_id: &str,
    data: &serde_json::Value,
) -> Option<SubagentInfo> {
    let path = data["transcript_path"]
        .as_str()
        .or_else(|| data["session_path"].as_str())
        .or_else(|| data["session_file"].as_str())?;
    codex_session_meta_from_path(session_id, Path::new(path))
}

/// Reads subagent lineage from a transcript file, attributing only records
/// that belong to `session_id`.
///
/// Codex appends child spawn records (`session_meta` entries whose `id` is
/// the child while `parent_thread_id` points at the spawner) into the
/// parent's own transcript. Without the id check the parent would inherit
/// its child's lineage and look like its own subagent.
fn codex_session_meta_from_path(session_id: &str, path: &Path) -> Option<SubagentInfo> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut contents = String::new();
    (&mut file)
        .take(256 * 1024)
        .read_to_string(&mut contents)
        .ok()?;
    contents.lines().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        if entry["type"].as_str()? != "session_meta" {
            return None;
        }
        let payload = &entry["payload"];
        if payload["id"].as_str() != Some(session_id) {
            return None;
        }
        subagent_info_from_value(payload)
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
    use super::codex_session_meta_from_path;

    fn write_transcript(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn ignores_child_spawn_records_in_parent_transcript() {
        let path = write_transcript(
            "agent-dbus-codex-subagent-test.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"session_id":"ses_parent","id":"ses_parent","thread_source":"user"}}"#,
                r#"{"type":"session_meta","payload":{"session_id":"ses_parent","id":"ses_child","parent_thread_id":"ses_parent","source":{"subagent":{"thread_spawn":{"parent_thread_id":"ses_parent"}}}}}"#,
            ],
        );

        // The parent's own record is top-level: no subagent info.
        assert!(codex_session_meta_from_path("ses_parent", &path).is_none());
        // The child's spawn record attributes to the child, not the parent.
        let child = codex_session_meta_from_path("ses_child", &path).unwrap();
        assert_eq!(child.parent_session_id, "ses_parent");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_self_parenting() {
        assert!(
            super::codex_subagent_info(
                "codex",
                "ses_1",
                &serde_json::json!({ "parent_thread_id": "ses_1" }),
            )
            .is_none()
        );
    }
}
