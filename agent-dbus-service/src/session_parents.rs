use std::path::Path;

use agent_dbus_core::path::session_key;
use tokio::time::{Duration, sleep};
use tracing::info;

use crate::session_store::remove_session;
use crate::{EndedSessions, SessionParents};

const PARENT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Removes session objects whose owning agent process has exited.
///
/// Not every agent has a reliable end-of-session hook: Codex has no
/// `SessionEnd` command hook at all, and hook-based cleanup never runs for any
/// agent when the terminal is killed. Without this, dead sessions accumulate on
/// the bus and compete with the live session for the same window id.
pub(crate) fn start_parent_watcher(
    conn: zbus::Connection,
    ended: EndedSessions,
    session_parents: SessionParents,
) {
    tokio::spawn(async move {
        loop {
            sleep(PARENT_POLL_INTERVAL).await;
            let watched: Vec<(String, u32)> = session_parents
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
                    session_parents.lock().await.remove(&key);
                    continue;
                };
                let still_current =
                    session_parents.lock().await.get(&key).copied() == Some(parent_pid);
                if still_current {
                    remove_session(&conn, &ended, &session_parents, agent_name, session_id).await;
                    info!(
                        agent_name,
                        session_id, parent_pid, "removed session after agent process exited"
                    );
                }
            }
        }
    });
}

/// Records the agent process owning a session, so [`start_parent_watcher`] can
/// reap it later.
///
/// `parent_pid` is only ever set by `agent-hook` when it positively identified
/// an ancestor process named after the agent. An unidentified ancestor is left
/// unwatched rather than guessed at, because reaping on the wrong pid — a
/// short-lived wrapper shell, say — would delete sessions that are still live.
pub(crate) async fn maybe_watch_parent(
    session_parents: &SessionParents,
    agent_name: &str,
    session_id: &str,
    parent_pid: Option<u32>,
) {
    if session_id == "unknown" {
        return;
    }

    let Some(parent_pid) = parent_pid else {
        return;
    };

    let key = session_key(agent_name, session_id);
    let mut parents = session_parents.lock().await;
    if parents.get(&key) == Some(&parent_pid) {
        return;
    }
    parents.insert(key, parent_pid);
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}
