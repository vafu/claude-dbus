use std::collections::HashMap;

use agent_dbus_core::path::{agent_session_node_key, session_path};
use tracing::warn;
use zbus::Proxy;

const LOCUS_BUS: &str = "org.rsynapse.Locus";
const LOCUS_PATH: &str = "/org/rsynapse/Locus";
const LOCUS_INTERFACE: &str = "org.rsynapse.Locus.Relations1";
const WINDOW_AGENT_RELATION: &str = "org.rsynapse.window.agent-session";
const APP_INSTANCE_AGENT_RELATION: &str = "org.rsynapse.app-instance.agent-session";
const AGENT_SESSION_KIND: &str = "org.rsynapse.agent.session.id";
const APP_INSTANCE_KIND: &str = "org.rsynapse.app-instance.id";
const NIRI_WINDOW_KIND: &str = "org.rsynapse.niri.window.id";

type RelationEndpoint = HashMap<String, String>;

pub(crate) async fn link_session(
    agent_name: &str,
    session_id: &str,
    app_instance_id: Option<&str>,
    window_id: Option<&str>,
) {
    let Err(err) = try_link_session(agent_name, session_id, app_instance_id, window_id).await
    else {
        return;
    };
    warn!(%err, agent = %agent_name, session_id = %session_id, "failed to update locus session links");
}

pub(crate) async fn unlink_session(
    agent_name: &str,
    session_id: &str,
    app_instance_id: Option<&str>,
    window_id: Option<&str>,
) {
    let Err(err) = try_unlink_session(agent_name, session_id, app_instance_id, window_id).await
    else {
        return;
    };
    warn!(%err, agent = %agent_name, session_id = %session_id, "failed to remove locus session links");
}

async fn try_link_session(
    agent_name: &str,
    session_id: &str,
    app_instance_id: Option<&str>,
    window_id: Option<&str>,
) -> zbus::Result<()> {
    let Some(target) = agent_session_ref(agent_name, session_id) else {
        return Ok(());
    };
    let metadata = session_metadata(agent_name, session_id);
    let connection = zbus::Connection::session().await?;
    let proxy = locus_proxy(&connection).await?;

    if let Some(subject) = window_ref(window_id) {
        set_one(&proxy, &subject, WINDOW_AGENT_RELATION, &target, &metadata).await?;
    }
    if let Some(subject) = app_instance_ref(app_instance_id) {
        set_one(
            &proxy,
            &subject,
            APP_INSTANCE_AGENT_RELATION,
            &target,
            &metadata,
        )
        .await?;
    }

    Ok(())
}

async fn try_unlink_session(
    agent_name: &str,
    session_id: &str,
    app_instance_id: Option<&str>,
    window_id: Option<&str>,
) -> zbus::Result<()> {
    let Some(target) = agent_session_ref(agent_name, session_id) else {
        return Ok(());
    };
    let connection = zbus::Connection::session().await?;
    let proxy = locus_proxy(&connection).await?;

    if let Some(subject) = window_ref(window_id) {
        unset(&proxy, &subject, WINDOW_AGENT_RELATION, &target).await?;
    }
    if let Some(subject) = app_instance_ref(app_instance_id) {
        unset(&proxy, &subject, APP_INSTANCE_AGENT_RELATION, &target).await?;
    }

    Ok(())
}

async fn locus_proxy(connection: &zbus::Connection) -> zbus::Result<Proxy<'_>> {
    Proxy::new(connection, LOCUS_BUS, LOCUS_PATH, LOCUS_INTERFACE).await
}

async fn set_one(
    proxy: &Proxy<'_>,
    subject: &RelationEndpoint,
    relation: &str,
    target: &RelationEndpoint,
    metadata: &HashMap<String, String>,
) -> zbus::Result<()> {
    proxy
        .call_method("SetOne", &(subject, relation, target, metadata))
        .await
        .map(|_| ())
}

async fn unset(
    proxy: &Proxy<'_>,
    subject: &RelationEndpoint,
    relation: &str,
    target: &RelationEndpoint,
) -> zbus::Result<()> {
    proxy
        .call_method("Unset", &(subject, relation, target))
        .await
        .map(|_| ())
}

fn session_metadata(agent_name: &str, session_id: &str) -> HashMap<String, String> {
    HashMap::from([
        ("agent".to_string(), agent_name.to_string()),
        ("session_id".to_string(), session_id.to_string()),
        (
            "session_path".to_string(),
            session_path(agent_name, session_id).to_string(),
        ),
    ])
}

fn agent_session_ref(agent_name: &str, session_id: &str) -> Option<RelationEndpoint> {
    non_empty(session_id).map(|_| {
        stable_key(
            AGENT_SESSION_KIND,
            agent_session_node_key(agent_name, session_id),
        )
    })
}

fn window_ref(window_id: Option<&str>) -> Option<RelationEndpoint> {
    normalized_ref("niri-window", NIRI_WINDOW_KIND, window_id)
}

fn app_instance_ref(app_instance_id: Option<&str>) -> Option<RelationEndpoint> {
    normalized_ref("app-instance", APP_INSTANCE_KIND, app_instance_id)
}

fn normalized_ref(prefix: &str, kind: &str, value: Option<&str>) -> Option<RelationEndpoint> {
    let value = non_empty(value?)?;
    let key = value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix(':'))
        .or_else(|| value.strip_prefix("window:"))
        .unwrap_or(value);
    non_empty(key).map(|key| stable_key(kind, key))
}

fn stable_key(kind: impl Into<String>, id: impl Into<String>) -> RelationEndpoint {
    HashMap::from([
        ("type".to_string(), "stable-key".to_string()),
        ("kind".to_string(), kind.into()),
        ("id".to_string(), id.into()),
    ])
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}
