use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_dbus_core::constants::{BUS_NAME, ROOT_PATH, SESSION_INTERFACE};
use agent_dbus_core::path::{agent_session_node_key, safe_path_segment};
use tokio::time::sleep;
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::OwnedValue;

const ROOT_ENV: &str = "LOCUS_ROOT";
const LEGACY_ROOT_ENV: &str = "LOCUSFS_ROOT";
const LEGACY_MOUNT_ENV: &str = "LOCUS_MOUNT";
const DEFAULT_ROOT: &str = "/tmp/locusfs";
const SUBAGENT_SESSION_RELATION: &str = "subagent-session";
const SESSION_PREFIX: &str = "/io/github/AgentDBus/sessions/";

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
    context_pct: f64,
    task_complete: bool,
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

    fn node(&self) -> NodeRef {
        NodeRef::new("agent-session", self.node_key())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NodeRef {
    kind: String,
    key: String,
}

impl NodeRef {
    fn new(kind: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            key: key.into(),
        }
    }

    fn parse(subject: &str) -> Option<Self> {
        let (kind, key) = subject.split_once(':')?;
        Some(Self::new(kind, key))
    }

    fn path(&self, root: &Path) -> PathBuf {
        root.join(&self.kind).join(locusfs_key(&self.key))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct MirrorArtifacts {
    agent_nodes: HashSet<PathBuf>,
    owned_app_nodes: HashSet<PathBuf>,
    links: HashSet<PathBuf>,
}

struct LocusFs {
    root: PathBuf,
}

impl LocusFs {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn is_ready(&self) -> bool {
        fs::metadata(self.root.join("watch"))
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    }

    fn ensure_node(&self, node: &NodeRef) -> io::Result<()> {
        fs::create_dir_all(node.path(&self.root))
    }

    fn set_property(&self, node: &NodeRef, key: &str, value: &str) -> io::Result<PathBuf> {
        self.ensure_node(node)?;
        let path = node.path(&self.root).join(key);
        fs::write(&path, value)?;
        Ok(path)
    }

    fn set_link(&self, source: &NodeRef, relation: &str, target: &NodeRef) -> io::Result<PathBuf> {
        self.ensure_node(source)?;
        self.ensure_node(target)?;

        let path = source.path(&self.root).join(relation);
        let target_path = target.path(&self.root);
        let link_target = relative_link_target(path.parent().unwrap_or(&self.root), &target_path);
        replace_symlink(&path, &link_target)?;
        Ok(path)
    }

    fn set_collection_link(
        &self,
        source: &NodeRef,
        relation: &str,
        target: &NodeRef,
    ) -> io::Result<PathBuf> {
        self.ensure_node(source)?;
        self.ensure_node(target)?;

        let relation_dir = source.path(&self.root).join(relation);
        fs::create_dir_all(&relation_dir)?;
        let path = relation_dir.join(locusfs_key(&target.key));
        let target_path = target.path(&self.root);
        let link_target = relative_link_target(&relation_dir, &target_path);
        replace_symlink(&path, &link_target)?;
        Ok(path)
    }

    fn remove_artifacts(&self, artifacts: &MirrorArtifacts) {
        for link in &artifacts.links {
            remove_symlink(link);
        }
        for node in &artifacts.agent_nodes {
            remove_owned_dir(node);
        }
        for node in &artifacts.owned_app_nodes {
            remove_owned_dir(node);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let connection = zbus::Connection::session().await?;
    let locusfs = LocusFs::new(root());
    let mut known = MirrorArtifacts::default();
    let mut was_ready = None;

    info!(root = %locusfs.root.display(), "Starting agent-dbus locusfs symlink mirror...");
    loop {
        if !locusfs.is_ready() {
            if was_ready != Some(false) {
                warn!(
                    root = %locusfs.root.display(),
                    "locusfs root is not mounted; skipping agent-dbus mirror passes"
                );
            }
            was_ready = Some(false);
            sleep(Duration::from_millis(1000)).await;
            continue;
        }
        if was_ready == Some(false) {
            info!(root = %locusfs.root.display(), "locusfs root is mounted; resuming agent-dbus mirror");
        }
        was_ready = Some(true);

        match mirror_once(&connection, &locusfs).await {
            Ok(current) => {
                remove_stale_artifacts(&locusfs, &known, &current);
                known = current;
            }
            Err(err) => warn!(%err, "failed to mirror agent-dbus sessions"),
        }
        sleep(Duration::from_millis(1000)).await;
    }
}

async fn mirror_once(
    connection: &zbus::Connection,
    locusfs: &LocusFs,
) -> zbus::Result<MirrorArtifacts> {
    let object_manager = ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)?
        .path(ROOT_PATH)?
        .build()
        .await?;
    let objects = object_manager.get_managed_objects().await?;
    let mut artifacts = MirrorArtifacts::default();

    for (path, interfaces) in objects {
        let Some((_interface, properties)) = interfaces
            .iter()
            .find(|(interface, _)| interface.as_str() == SESSION_INTERFACE)
        else {
            continue;
        };
        let session = session_from_properties(path.as_str(), properties);
        if session.session_id.is_empty() || session.agent.is_empty() {
            continue;
        }
        mirror_session(locusfs, &session, &mut artifacts);
    }

    Ok(artifacts)
}

fn mirror_session(locusfs: &LocusFs, session: &SessionMirror, artifacts: &mut MirrorArtifacts) {
    let node = session.node();
    artifacts.agent_nodes.insert(node.path(&locusfs.root));

    set_property(locusfs, &node, "kind", "agent-session");
    set_property(locusfs, &node, "id", &session.session_id);
    set_property(locusfs, &node, "agent", &session.agent);
    set_property(locusfs, &node, "raw_title", &session.raw_title);
    set_property(locusfs, &node, "model", &session.model);
    set_property(locusfs, &node, "cwd", &session.cwd);
    set_property(locusfs, &node, "state", &session.state);
    set_property(
        locusfs,
        &node,
        "context_pct",
        &session.context_pct.to_string(),
    );
    set_property(
        locusfs,
        &node,
        "task_complete",
        bool_str(session.task_complete),
    );
    set_property(
        locusfs,
        &node,
        "requires_attention",
        bool_str(session.requires_attention),
    );
    set_property(locusfs, &node, "is_subagent", bool_str(session.is_subagent));
    set_property(
        locusfs,
        &node,
        "parent_session_id",
        &session.parent_session_id,
    );
    set_property(locusfs, &node, "agent_nickname", &session.agent_nickname);
    set_property(locusfs, &node, "agent_role", &session.agent_role);

    mirror_project(locusfs, &node, &session.cwd, artifacts);
    mirror_window_link(locusfs, session, &node, artifacts);
    mirror_subagent_link(locusfs, session, &node, artifacts);
}

fn mirror_window_link(
    locusfs: &LocusFs,
    session: &SessionMirror,
    node: &NodeRef,
    artifacts: &mut MirrorArtifacts,
) {
    let Some(app_instance) = app_instance_node(session) else {
        return;
    };

    if session.app_instance_id.is_empty() {
        artifacts
            .owned_app_nodes
            .insert(app_instance.path(&locusfs.root));
    }

    set_property(locusfs, &app_instance, "kind", "app-instance");
    set_property(locusfs, &app_instance, "name", &session.agent);
    set_property(
        locusfs,
        &app_instance,
        "icon",
        &safe_path_segment(&session.agent),
    );

    if !session.window_id.is_empty() {
        let window = NodeRef::new("window", &session.window_id);
        set_link(locusfs, &window, "app-instance", &app_instance, artifacts);
    }
    set_link(locusfs, &app_instance, "agent-session", node, artifacts);
}

fn app_instance_node(session: &SessionMirror) -> Option<NodeRef> {
    if let Some(node) = NodeRef::parse(&session.app_instance_id) {
        Some(node)
    } else if !session.app_instance_id.is_empty() {
        Some(NodeRef::new("app-instance", &session.app_instance_id))
    } else if !session.window_id.is_empty() {
        Some(NodeRef::new("app-instance", session.node_key()))
    } else {
        None
    }
}

fn mirror_subagent_link(
    locusfs: &LocusFs,
    session: &SessionMirror,
    node: &NodeRef,
    artifacts: &mut MirrorArtifacts,
) {
    if !session.is_subagent || session.parent_session_id.is_empty() {
        return;
    }
    let parent_key = agent_session_node_key(&session.agent, &session.parent_session_id);
    let parent = NodeRef::new("agent-session", parent_key);
    artifacts.agent_nodes.insert(parent.path(&locusfs.root));
    set_property(locusfs, &parent, "kind", "agent-session");
    set_property(locusfs, &parent, "id", &session.parent_session_id);
    set_collection_link(locusfs, &parent, SUBAGENT_SESSION_RELATION, node, artifacts);
}

fn mirror_project(
    locusfs: &LocusFs,
    session: &NodeRef,
    cwd: &str,
    artifacts: &mut MirrorArtifacts,
) {
    let Some(project) = project_for_cwd(cwd) else {
        return;
    };
    let subject = NodeRef::new("project", project.root.display().to_string());
    set_property(locusfs, &subject, "kind", "project");
    set_property(
        locusfs,
        &subject,
        "path",
        &project.root.display().to_string(),
    );
    set_property(locusfs, &subject, "name", &project.name);
    if let Some(icon) = project.icon.as_deref().filter(|icon| !icon.is_empty()) {
        set_property(locusfs, &subject, "icon", icon);
    }
    set_link(locusfs, session, "session-project", &subject, artifacts);
}

fn remove_stale_artifacts(locusfs: &LocusFs, known: &MirrorArtifacts, current: &MirrorArtifacts) {
    let stale = MirrorArtifacts {
        agent_nodes: known
            .agent_nodes
            .difference(&current.agent_nodes)
            .cloned()
            .collect(),
        owned_app_nodes: known
            .owned_app_nodes
            .difference(&current.owned_app_nodes)
            .cloned()
            .collect(),
        links: known.links.difference(&current.links).cloned().collect(),
    };
    locusfs.remove_artifacts(&stale);
}

fn session_from_properties(
    object_path: &str,
    properties: &std::collections::HashMap<String, OwnedValue>,
) -> SessionMirror {
    let (path_agent, path_session_id) = session_from_path(object_path).unwrap_or_default();
    SessionMirror {
        session_id: string_property(properties, "SessionId")
            .if_empty_then(|| path_session_id.clone()),
        agent: string_property(properties, "AgentName").if_empty_then(|| path_agent.clone()),
        app_instance_id: string_property(properties, "AppInstanceId"),
        window_id: string_property(properties, "WindowId"),
        raw_title: string_property(properties, "SessionTitle"),
        model: string_property(properties, "ModelName"),
        cwd: string_property(properties, "Cwd"),
        state: string_property(properties, "State"),
        context_pct: f64_property(properties, "ContextPct"),
        task_complete: bool_property(properties, "TaskComplete"),
        requires_attention: bool_property(properties, "RequiresAttention"),
        is_subagent: bool_property(properties, "IsSubagent"),
        parent_session_id: string_property(properties, "ParentSessionId"),
        agent_nickname: string_property(properties, "AgentNickname"),
        agent_role: string_property(properties, "AgentRole"),
    }
}

fn session_from_path(path: &str) -> Option<(String, String)> {
    let suffix = path.strip_prefix(SESSION_PREFIX)?;
    let (agent, session_id) = suffix.split_once('/')?;
    if agent.is_empty() || session_id.is_empty() {
        return None;
    }
    Some((agent.to_owned(), session_id.to_owned()))
}

trait StringExt {
    fn if_empty_then(self, fallback: impl FnOnce() -> String) -> String;
}

impl StringExt for String {
    fn if_empty_then(self, fallback: impl FnOnce() -> String) -> String {
        if self.is_empty() { fallback() } else { self }
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

fn f64_property(properties: &std::collections::HashMap<String, OwnedValue>, key: &str) -> f64 {
    properties
        .get(key)
        .and_then(|value| f64::try_from(value).ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

fn bool_str(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn locusfs_key(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

fn relative_link_target(from_dir: &Path, target: &Path) -> PathBuf {
    let Some(root) = common_root(from_dir, target) else {
        return target.to_path_buf();
    };
    let Ok(from_rel) = from_dir.strip_prefix(&root) else {
        return target.to_path_buf();
    };
    let Ok(target_rel) = target.strip_prefix(&root) else {
        return target.to_path_buf();
    };

    let mut relative = PathBuf::new();
    for _ in from_rel.components() {
        relative.push("..");
    }
    relative.push(target_rel);
    relative
}

fn common_root(left: &Path, right: &Path) -> Option<PathBuf> {
    let mut root = PathBuf::new();
    let mut found = false;
    for (left, right) in left.components().zip(right.components()) {
        if left != right {
            break;
        }
        root.push(left.as_os_str());
        found = true;
    }
    found.then_some(root)
}

fn set_property(locusfs: &LocusFs, node: &NodeRef, key: &str, value: &str) {
    if let Err(err) = locusfs.set_property(node, key, value) {
        warn!(
            node = %node.path(&locusfs.root).display(),
            key,
            %err,
            "failed to set locusfs property"
        );
    }
}

fn set_link(
    locusfs: &LocusFs,
    source: &NodeRef,
    relation: &str,
    target: &NodeRef,
    artifacts: &mut MirrorArtifacts,
) {
    match locusfs.set_link(source, relation, target) {
        Ok(path) => {
            artifacts.links.insert(path);
        }
        Err(err) => warn!(
            source = %source.path(&locusfs.root).display(),
            relation,
            target = %target.path(&locusfs.root).display(),
            %err,
            "failed to set locusfs link"
        ),
    }
}

fn set_collection_link(
    locusfs: &LocusFs,
    source: &NodeRef,
    relation: &str,
    target: &NodeRef,
    artifacts: &mut MirrorArtifacts,
) {
    match locusfs.set_collection_link(source, relation, target) {
        Ok(path) => {
            artifacts.links.insert(path);
        }
        Err(err) => warn!(
            source = %source.path(&locusfs.root).display(),
            relation,
            target = %target.path(&locusfs.root).display(),
            %err,
            "failed to set locusfs collection link"
        ),
    }
}

fn replace_symlink(link: &Path, target: &Path) -> io::Result<()> {
    match fs::read_link(link) {
        Ok(existing) if existing == target => return Ok(()),
        Ok(_) => fs::remove_file(link)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    unix_fs::symlink(target, link)
}

fn remove_symlink(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if let Err(err) = fs::remove_file(path) {
                warn!(path = %path.display(), %err, "failed to remove stale locusfs link");
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(err) => warn!(path = %path.display(), %err, "failed to stat stale locusfs link"),
    }
}

fn remove_owned_dir(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            debug!(path = %path.display(), "removing stale locusfs node");
            if let Err(err) = fs::remove_dir_all(path) {
                warn!(path = %path.display(), %err, "failed to remove stale locusfs node");
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(err) => warn!(path = %path.display(), %err, "failed to stat stale locusfs node"),
    }
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
    let cwd = fs::canonicalize(cwd).ok()?;
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
    fs::canonicalize(parent).ok()
}

fn read_project_metadata(root: &Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(root.join(".project.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

fn root() -> PathBuf {
    std::env::var_os(ROOT_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(LEGACY_ROOT_ENV).map(PathBuf::from))
        .or_else(|| std::env::var_os(LEGACY_MOUNT_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))
}

#[cfg(test)]
mod tests {
    use super::{LocusFs, NodeRef, locusfs_key, relative_link_target, session_from_path};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn locusfs_key_encodes_path_separators_and_reserved_chars() {
        assert_eq!(
            locusfs_key("/home/v47/proj/locus-shell"),
            "%2Fhome%2Fv47%2Fproj%2Flocus-shell"
        );
        assert_eq!(locusfs_key("codex/session:1%"), "codex%2Fsession%3A1%25");
    }

    #[test]
    fn node_path_uses_encoded_key_as_single_locusfs_entry() {
        let path = NodeRef::new("agent-session", "codex/session").path(Path::new("/tmp/rsynapse"));
        assert_eq!(
            path,
            Path::new("/tmp/rsynapse/agent-session/codex%2Fsession")
        );
    }

    #[test]
    fn session_from_path_uses_object_manager_suffix() {
        assert_eq!(
            session_from_path("/io/github/AgentDBus/sessions/codex/session_2d_1"),
            Some(("codex".to_owned(), "session_2d_1".to_owned()))
        );
    }

    #[test]
    fn relative_link_target_matches_locusfs_relation_shape() {
        assert_eq!(
            relative_link_target(
                Path::new("/tmp/rsynapse/window/4"),
                Path::new("/tmp/rsynapse/app-instance/codex_%2F4538-20318")
            ),
            Path::new("../../app-instance/codex_%2F4538-20318")
        );
        assert_eq!(
            relative_link_target(
                Path::new("/tmp/rsynapse/agent-session/codex%2Fsession"),
                Path::new("/tmp/rsynapse/project/%2Fhome%2Fv47%2Fproj%2Flocus-shell")
            ),
            Path::new("../../project/%2Fhome%2Fv47%2Fproj%2Flocus-shell")
        );
    }

    #[test]
    fn locusfs_ready_requires_watch_file() {
        let root = temp_path("agent-dbus-locusfs-proxy-ready");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let locusfs = LocusFs::new(root.clone());

        assert!(!locusfs.is_ready());

        fs::write(root.join("watch"), "").unwrap();
        assert!(locusfs.is_ready());

        fs::remove_dir_all(root).unwrap();
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
    }
}
