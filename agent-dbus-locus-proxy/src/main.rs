use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_dbus_core::constants::{BUS_NAME, ROOT_PATH, SESSION_INTERFACE};
use agent_dbus_core::path::{agent_session_node_key, safe_path_segment};
use locus::GraphWriteProxy;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::OwnedValue;

const SUBAGENT_SESSION_RELATION: &str = "subagent-session";

#[derive(Clone, Debug, Default)]
struct SessionMirror {
    session_id: String,
    agent: String,
    app_instance_id: String,
    window_id: String,
    raw_title: String,
    model: String,
    cwd: String,
    state: String,
    requires_attention: bool,
    is_subagent: bool,
    parent_session_id: String,
    agent_nickname: String,
    agent_role: String,
}

impl SessionMirror {
    fn node_key(&self) -> String {
        agent_session_node_key(&self.agent, &self.session_id)
    }

    fn node(&self) -> String {
        format!("agent-session:{}", self.node_key())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let connection = zbus::Connection::session().await?;
    let locus = GraphWriteProxy::new(&connection).await?;
    let mut known_nodes = HashSet::new();

    info!("Starting agent-dbus Locus proxy...");
    loop {
        match mirror_once(&connection, &locus).await {
            Ok(current_nodes) => {
                remove_stale_nodes(&locus, &mut known_nodes, &current_nodes).await;
                known_nodes = current_nodes;
            }
            Err(err) => warn!(%err, "failed to mirror agent-dbus sessions"),
        }
        sleep(Duration::from_millis(1000)).await;
    }
}

async fn mirror_once(
    connection: &zbus::Connection,
    locus: &GraphWriteProxy<'_>,
) -> zbus::Result<HashSet<String>> {
    let object_manager = ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)?
        .path(ROOT_PATH)?
        .build()
        .await?;
    let objects = object_manager.get_managed_objects().await?;
    let mut nodes = HashSet::new();

    for (_path, interfaces) in objects {
        let Some((_interface, properties)) = interfaces
            .iter()
            .find(|(interface, _)| interface.as_str() == SESSION_INTERFACE)
        else {
            continue;
        };
        let session = session_from_properties(properties);
        if session.session_id.is_empty() || session.agent.is_empty() {
            continue;
        }
        let node = session.node();
        mirror_session(locus, &session).await;
        nodes.insert(node);
    }

    Ok(nodes)
}

async fn mirror_session(locus: &GraphWriteProxy<'_>, session: &SessionMirror) {
    let node = session.node();
    set_property(locus, &node, "kind", "agent-session").await;
    set_property(locus, &node, "id", &session.session_id).await;
    set_property(locus, &node, "agent", &session.agent).await;
    set_property(locus, &node, "raw_title", &session.raw_title).await;
    set_property(locus, &node, "model", &session.model).await;
    set_property(locus, &node, "cwd", &session.cwd).await;
    set_property(locus, &node, "state", &session.state).await;
    set_property(
        locus,
        &node,
        "requires_attention",
        bool_str(session.requires_attention),
    )
    .await;
    set_property(locus, &node, "is_subagent", bool_str(session.is_subagent)).await;
    set_property(
        locus,
        &node,
        "parent_session_id",
        &session.parent_session_id,
    )
    .await;
    set_property(locus, &node, "agent_nickname", &session.agent_nickname).await;
    set_property(locus, &node, "agent_role", &session.agent_role).await;

    mirror_project(locus, &node, &session.cwd).await;
    mirror_window_link(locus, session, &node).await;
    mirror_subagent_link(locus, session, &node).await;
}

async fn mirror_window_link(locus: &GraphWriteProxy<'_>, session: &SessionMirror, node: &str) {
    let app_instance = if !session.app_instance_id.is_empty() {
        session.app_instance_id.clone()
    } else if !session.window_id.is_empty() {
        format!("app-instance:{}", session.node_key())
    } else {
        return;
    };

    set_property(locus, &app_instance, "kind", "app-instance").await;
    set_property(locus, &app_instance, "name", &session.agent).await;
    set_property(
        locus,
        &app_instance,
        "icon",
        &safe_path_segment(&session.agent),
    )
    .await;

    if !session.window_id.is_empty() {
        let window = format!("window:{}", session.window_id);
        set_link(locus, &window, "app-instance", &app_instance).await;
    }
    set_link(locus, &app_instance, "agent-session", node).await;
}

async fn mirror_subagent_link(locus: &GraphWriteProxy<'_>, session: &SessionMirror, node: &str) {
    if !session.is_subagent || session.parent_session_id.is_empty() {
        return;
    }
    let parent_key = agent_session_node_key(&session.agent, &session.parent_session_id);
    let parent = format!("agent-session:{parent_key}");
    set_property(locus, &parent, "kind", "agent-session").await;
    set_property(locus, &parent, "id", &session.parent_session_id).await;
    set_link(locus, &parent, SUBAGENT_SESSION_RELATION, node).await;
}

async fn mirror_project(locus: &GraphWriteProxy<'_>, session: &str, cwd: &str) {
    let Some(project) = project_for_cwd(cwd) else {
        return;
    };
    let subject = format!("project:{}", project.root.display());
    set_property(locus, &subject, "kind", "project").await;
    set_property(locus, &subject, "path", &project.root.display().to_string()).await;
    set_property(locus, &subject, "name", &project.name).await;
    if let Some(icon) = project.icon.as_deref().filter(|icon| !icon.is_empty()) {
        set_property(locus, &subject, "icon", icon).await;
    }
    set_link(locus, session, "session-project", &subject).await;
}

async fn remove_stale_nodes(
    locus: &GraphWriteProxy<'_>,
    known_nodes: &mut HashSet<String>,
    current_nodes: &HashSet<String>,
) {
    for node in known_nodes.difference(current_nodes) {
        debug!(node, "removing stale agent session node");
        let _ = locus.delete_node(node).await;
    }
}

fn session_from_properties(
    properties: &std::collections::HashMap<String, OwnedValue>,
) -> SessionMirror {
    SessionMirror {
        session_id: string_property(properties, "SessionId"),
        agent: string_property(properties, "AgentName"),
        app_instance_id: string_property(properties, "AppInstanceId"),
        window_id: string_property(properties, "WindowId"),
        raw_title: string_property(properties, "SessionTitle"),
        model: string_property(properties, "ModelName"),
        cwd: string_property(properties, "Cwd"),
        state: string_property(properties, "State"),
        requires_attention: bool_property(properties, "RequiresAttention"),
        is_subagent: bool_property(properties, "IsSubagent"),
        parent_session_id: string_property(properties, "ParentSessionId"),
        agent_nickname: string_property(properties, "AgentNickname"),
        agent_role: string_property(properties, "AgentRole"),
    }
}

fn string_property(
    properties: &std::collections::HashMap<String, OwnedValue>,
    key: &str,
) -> String {
    properties
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_string)
        .unwrap_or_default()
}

fn bool_property(properties: &std::collections::HashMap<String, OwnedValue>, key: &str) -> bool {
    properties
        .get(key)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn bool_str(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

async fn set_property(locus: &GraphWriteProxy<'_>, subject: &str, key: &str, value: &str) {
    let _ = locus.set_property(subject, key, value).await;
}

async fn set_link(locus: &GraphWriteProxy<'_>, source: &str, relation: &str, target: &str) {
    let _ = locus.set_link(source, relation, target).await;
}

struct Project {
    root: PathBuf,
    name: String,
    icon: Option<String>,
}

fn project_for_cwd(cwd: &str) -> Option<Project> {
    if cwd.is_empty() {
        return None;
    }
    let cwd = std::fs::canonicalize(cwd).ok()?;
    let parent = project_parent()?;
    let relative = cwd.strip_prefix(&parent).ok()?;
    let project_name = relative.components().next()?.as_os_str().to_str()?;
    if project_name.is_empty() {
        return None;
    }

    let root = parent.join(project_name);
    let metadata = read_project_metadata(&root);
    Some(Project {
        root,
        name: metadata
            .as_ref()
            .and_then(|value| json_string(value, "name"))
            .unwrap_or_else(|| project_name.to_string()),
        icon: metadata
            .as_ref()
            .and_then(|value| json_string(value, "icon")),
    })
}

fn project_parent() -> Option<PathBuf> {
    let parent = std::env::var_os("PROJECT_PARENT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join("proj")))?;
    std::fs::canonicalize(parent).ok()
}

fn read_project_metadata(root: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(root.join(".project.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}
