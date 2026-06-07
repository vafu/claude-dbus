use std::path::Path;
use std::process::Command;

use tokio::time::{Duration, sleep};
use tracing::warn;

use crate::session_store::update_existing_session;

use super::artifacts::{codex_session_file, codex_session_index_file, read_file_tail};

const CODEX_THREAD_TITLE_ROLLOUT_TAIL_READ_MAX_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) fn start_codex_title_watcher(conn: zbus::Connection) {
    let Some(path) = codex_session_index_file() else {
        return;
    };

    tokio::spawn(async move {
        let mut last_seen = None;
        loop {
            sleep(Duration::from_millis(500)).await;
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let seen = (
                metadata.len(),
                metadata.modified().ok(),
                metadata.created().ok(),
            );
            if last_seen.as_ref() == Some(&seen) {
                continue;
            }
            last_seen = Some(seen);

            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in contents.lines() {
                let Some((session_id, title)) = parse_codex_session_index_line(line) else {
                    continue;
                };
                if title.is_empty() {
                    continue;
                }
                if let Err(err) = update_existing_session(&conn, "codex", &session_id, |d| {
                    d.session_title = title.clone();
                })
                .await
                {
                    warn!(
                        %err,
                        %session_id,
                        "D-Bus operation failed while applying Codex title update"
                    );
                }
            }
        }
    });
}

pub(crate) fn codex_thread_title(session_id: &str) -> Option<String> {
    codex_thread_title_from_state(session_id)
        .or_else(|| codex_thread_title_from_session_index(session_id))
        .or_else(|| codex_thread_title_from_rollout(session_id))
}

fn codex_thread_title_from_state(session_id: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let db = Path::new(&home).join(".codex/state_5.sqlite");
    let query = format!(
        "select title from threads where id = '{}';",
        sqlite_string_literal(session_id)
    );
    let output = Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(query)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_codex_thread_title_json(&stdout)
}

pub(crate) fn parse_codex_thread_title_json(output: &str) -> Option<String> {
    let rows: serde_json::Value = serde_json::from_str(output).ok()?;
    rows.as_array()?
        .first()?
        .get("title")?
        .as_str()
        .map(str::to_string)
        .filter(|title| !title.is_empty())
}

fn sqlite_string_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn codex_thread_title_from_session_index(session_id: &str) -> Option<String> {
    let path = codex_session_index_file()?;
    let contents = std::fs::read_to_string(path).ok()?;
    contents
        .lines()
        .rev()
        .filter_map(parse_codex_session_index_line)
        .find_map(|(id, title)| (id == session_id && !title.is_empty()).then_some(title))
}

pub(crate) fn parse_codex_session_index_line(line: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let id = value["id"].as_str()?.to_string();
    let title = value["thread_name"]
        .as_str()
        .or_else(|| value["title"].as_str())?
        .to_string();
    Some((id, title))
}

fn codex_thread_title_from_rollout(session_id: &str) -> Option<String> {
    let path = codex_session_file(session_id)?;
    let contents = read_file_tail(&path, CODEX_THREAD_TITLE_ROLLOUT_TAIL_READ_MAX_BYTES)?;
    contents.lines().rev().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        if entry["type"].as_str()? != "event_msg" {
            return None;
        }
        let payload = &entry["payload"];
        if payload["type"].as_str()? != "thread_name_updated" {
            return None;
        }
        if payload["thread_id"].as_str()? != session_id {
            return None;
        }
        payload["thread_name"].as_str().map(str::to_string)
    })
}
