use tracing::{debug, warn};

use crate::dbus::{self, SessionObject};
use crate::{CodexSessionParents, EndedSessions};
use agent_dbus_core::path::{session_key, session_path};

pub(crate) use crate::dbus::{
    emit_elicitation, emit_elicitation_with_details, emit_elicitation_with_id,
    emit_elicitation_with_id_and_details, emit_notification,
};

pub(crate) async fn create_session(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
) -> zbus::Result<()> {
    dbus::create_session(conn, agent_name, session_id).await
}

pub(crate) async fn update_session(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    f: impl FnOnce(&mut SessionObject),
) -> zbus::Result<()> {
    dbus::update_session(conn, agent_name, session_id, f).await
}

pub(crate) async fn update_existing_session(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    f: impl FnOnce(&mut SessionObject),
) -> zbus::Result<()> {
    dbus::update_existing_session(conn, agent_name, session_id, f).await
}

pub(crate) async fn remove_session(
    conn: &zbus::Connection,
    ended: &EndedSessions,
    codex_session_parents: &CodexSessionParents,
    agent_name: &str,
    session_id: &str,
) {
    let key = session_key(agent_name, session_id);
    ended.lock().await.insert(key.clone());
    codex_session_parents.lock().await.remove(&key);
    let path = session_path(agent_name, session_id);
    if let Ok(iface_ref) = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    {
        let cancelled = iface_ref.get_mut().await.cancel_pending_requests();
        if cancelled > 0 {
            debug!(%session_id, cancelled, "cancelled pending requests for removed session");
        }
    }
    match conn.object_server().remove::<SessionObject, _>(&path).await {
        Ok(_) => {}
        Err(err) => warn!(%err, %session_id, "failed to remove session object"),
    }
}

pub(crate) async fn subagent_parent_session_id(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
) -> Option<String> {
    let path = session_path(agent_name, session_id);
    let Ok(iface_ref) = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    else {
        return None;
    };
    let iface = iface_ref.get().await;
    (iface.is_subagent && !iface.parent_session_id.is_empty())
        .then(|| iface.parent_session_id.clone())
}

pub(crate) fn log_zbus_result(result: zbus::Result<()>, action: &str, session_id: &str) {
    if let Err(err) = result {
        warn!(%err, %session_id, action, "D-Bus operation failed");
    }
}
