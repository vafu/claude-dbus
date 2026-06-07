use std::path::Path;

use agent_dbus_core::path::session_key;
use tokio::time::{Duration, sleep};
use tracing::info;

use crate::session_store::remove_session;
use crate::{CodexSessionParents, EndedSessions};

pub(crate) fn start_codex_parent_watcher(
    conn: zbus::Connection,
    ended: EndedSessions,
    codex_session_parents: CodexSessionParents,
) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(5)).await;
            let watched: Vec<(String, u32)> = codex_session_parents
                .lock()
                .await
                .iter()
                .map(|(key, pid)| (key.clone(), *pid))
                .collect();

            for (key, parent_pid) in watched {
                if process_exists(parent_pid) {
                    continue;
                }
                let Some((agent_name, session_id)) = key.split_once(':') else {
                    codex_session_parents.lock().await.remove(&key);
                    continue;
                };
                let still_current =
                    codex_session_parents.lock().await.get(&key).copied() == Some(parent_pid);
                if still_current {
                    remove_session(
                        &conn,
                        &ended,
                        &codex_session_parents,
                        agent_name,
                        session_id,
                    )
                    .await;
                    info!(
                        session_id,
                        parent_pid, "removed codex session after parent process exited"
                    );
                }
            }
        }
    });
}

pub(crate) async fn maybe_watch_codex_parent(
    codex_session_parents: &CodexSessionParents,
    agent_name: &str,
    session_id: &str,
    parent_pid: Option<u32>,
) {
    if agent_name != "codex" || session_id == "unknown" {
        return;
    }

    let Some(parent_pid) = parent_pid else {
        return;
    };

    let key = session_key(agent_name, session_id);
    let mut parents = codex_session_parents.lock().await;
    if parents.get(&key) == Some(&parent_pid) {
        return;
    }
    parents.insert(key, parent_pid);
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}
