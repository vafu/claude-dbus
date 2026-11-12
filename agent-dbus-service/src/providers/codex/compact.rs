use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use tokio::time::{Duration, sleep};
use tracing::warn;

use crate::providers::codex::artifacts::codex_log_files;
use crate::session_store::update_session;
use crate::types::SessionState;

pub(crate) fn start_codex_compact_watcher(conn: zbus::Connection) {
    if codex_log_files().is_empty() {
        return;
    }

    tokio::spawn(async move {
        // One read offset per Codex home's log: profiles run side by side and
        // each keeps its own compact lifecycle lines. The set is re-discovered
        // each tick so a profile started after the service is still watched.
        let mut offsets: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();

        loop {
            sleep(Duration::from_millis(500)).await;
            let paths = codex_log_files();
            for path in &paths {
                let offset = offsets.entry(path.clone()).or_insert(0);
                let Ok(metadata) = std::fs::metadata(path) else {
                    continue;
                };
                if metadata.len() < *offset {
                    *offset = 0;
                }
                if metadata.len() == *offset {
                    continue;
                }

                let Ok(mut file) = std::fs::File::open(path) else {
                    continue;
                };
                if file.seek(SeekFrom::Start(*offset)).is_err() {
                    continue;
                }
                let mut chunk = String::new();
                if file.read_to_string(&mut chunk).is_err() {
                    continue;
                }
                *offset = metadata.len();

                for line in chunk.lines() {
                    if let Some(event) = parse_codex_compact_log_line(line) {
                        apply_codex_compact_log_event(&conn, event).await;
                    }
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
        // Completion is sticky until the next turn starts. The compact log
        // watcher lags hook delivery, so a close line for a turn that already
        // stopped must not resurrect it as `thinking`.
        if d.task_complete {
            return;
        }
        if event.active {
            d.state = SessionState::Compacting;
            d.task_complete = false;
            d.clear_attention_reasons();
        } else if d.state == SessionState::Compacting {
            // Compact runs inside the turn; the agent resumes working unless a
            // Stop/Interrupt follows. This mirrors the PostCompact hook.
            d.state = SessionState::Thinking;
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
