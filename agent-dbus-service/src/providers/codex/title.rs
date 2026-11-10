use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

use tokio::time::{Duration, sleep};
use tracing::warn;

use crate::session_store::update_existing_session;

use super::artifacts::{
    codex_homes, codex_session_file, codex_session_index_files, read_file_tail,
};

const CODEX_THREAD_TITLE_ROLLOUT_TAIL_READ_MAX_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) fn start_codex_title_watcher(conn: zbus::Connection) {
    if codex_homes().is_empty() {
        return;
    }

    tokio::spawn(async move {
        let mut last_seen: std::collections::HashMap<
            PathBuf,
            (u64, Option<SystemTime>, Option<SystemTime>),
        > = std::collections::HashMap::new();
        loop {
            sleep(Duration::from_millis(500)).await;
            // Re-discover each tick so a profile that gains an index file
            // after the service starts is still picked up.
            let paths = codex_session_index_files();
            for path in &paths {
                let Ok(metadata) = std::fs::metadata(path) else {
                    continue;
                };
                let seen = (
                    metadata.len(),
                    metadata.modified().ok(),
                    metadata.created().ok(),
                );
                if last_seen.get(path) == Some(&seen) {
                    continue;
                }
                last_seen.insert(path.clone(), seen);

                let Ok(contents) = std::fs::read_to_string(path) else {
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
        }
    });
}

pub(crate) fn codex_thread_title(session_id: &str) -> Option<String> {
    codex_thread_title_from_state(session_id)
        .or_else(|| codex_thread_title_from_session_index(session_id))
        .or_else(|| codex_thread_title_from_rollout(session_id))
}

fn codex_thread_title_from_state(session_id: &str) -> Option<String> {
    let query = format!(
        "select title from threads where id = '{}';",
        sqlite_string_literal(session_id)
    );
    for home in codex_homes() {
        let db = home.join("state_5.sqlite");
        if !db.is_file() {
            continue;
        }
        let Ok(output) = Command::new("sqlite3")
            .arg("-readonly")
            .arg("-json")
            .arg(&db)
            .arg(&query)
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(stdout) = String::from_utf8(output.stdout) else {
            continue;
        };
        if let Some(title) = parse_codex_thread_title_json(&stdout) {
            return Some(title);
        }
    }
    None
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
    for path in codex_session_index_files() {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(title) = contents
            .lines()
            .rev()
            .filter_map(parse_codex_session_index_line)
            .find_map(|(id, title)| (id == session_id && !title.is_empty()).then_some(title))
        {
            return Some(title);
        }
    }
    None
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
