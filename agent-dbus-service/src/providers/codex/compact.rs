use std::io::{Read, Seek, SeekFrom};

use tokio::time::{Duration, sleep};
use tracing::warn;

use crate::providers::codex::artifacts::codex_log_file;
use crate::session_store::update_session;
use crate::types::SessionState;

pub(crate) fn start_codex_compact_watcher(conn: zbus::Connection) {
    let Some(path) = codex_log_file() else {
        return;
    };

    tokio::spawn(async move {
        let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        loop {
            sleep(Duration::from_millis(500)).await;
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() < offset {
                offset = 0;
            }
            if metadata.len() == offset {
                continue;
            }

            let Ok(mut file) = std::fs::File::open(&path) else {
                continue;
            };
            if file.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut chunk = String::new();
            if file.read_to_string(&mut chunk).is_err() {
                continue;
            }
            offset = metadata.len();

            for line in chunk.lines() {
                if let Some(event) = parse_codex_compact_log_line(line) {
                    apply_codex_compact_log_event(&conn, event).await;
                }
            }
        }
    });
}

#[derive(Debug, PartialEq)]
pub(crate) struct CodexCompactLogEvent {
    pub(crate) session_id: String,
    pub(crate) active: bool,
}

pub(crate) fn parse_codex_compact_log_line(line: &str) -> Option<CodexCompactLogEvent> {
    if !line.contains("codex.op=\"compact\"") {
        return None;
    }

    let active = if line.contains("codex_core::session::handlers: new") {
        true
    } else if line.contains("codex_core::session::handlers: close") {
        false
    } else {
        return None;
    };

    Some(CodexCompactLogEvent {
        session_id: parse_thread_id(line)?.to_string(),
        active,
    })
}

fn parse_thread_id(line: &str) -> Option<&str> {
    let start = line.find("thread_id=")? + "thread_id=".len();
    let rest = &line[start..];
    let end = rest.find('}')?;
    Some(&rest[..end])
}

async fn apply_codex_compact_log_event(conn: &zbus::Connection, event: CodexCompactLogEvent) {
    if let Err(err) = update_session(conn, "codex", &event.session_id, |d| {
        if event.active {
            d.state = SessionState::Compacting;
            d.task_complete = false;
            d.clear_attention_reasons();
        } else if d.state == SessionState::Compacting {
            d.state = SessionState::Idle;
            d.task_complete = true;
            d.clear_attention_reasons();
        }
    })
    .await
    {
        warn!(
            %err,
            session_id = %event.session_id,
            "D-Bus operation failed while applying Codex compact event"
        );
    }
}
