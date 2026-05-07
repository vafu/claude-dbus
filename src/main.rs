use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::connection;

use agent_dbus::constants::{BUS_NAME, ROOT_PATH, socket_path};

mod dbus;
mod hooks;
mod types;

pub type EndedSessions = Arc<Mutex<ExpiringSessionSet>>;
pub type CodexSessionParents = Arc<Mutex<HashMap<String, u32>>>;

pub struct ExpiringSessionSet {
    entries: HashMap<String, Instant>,
    order: VecDeque<(String, Instant)>,
    max_entries: usize,
    ttl: Duration,
}

impl ExpiringSessionSet {
    fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries,
            ttl,
        }
    }

    pub fn insert(&mut self, key: String) {
        self.prune();
        let inserted_at = Instant::now();
        self.entries.insert(key.clone(), inserted_at);
        self.order.push_back((key, inserted_at));
        while self.entries.len() > self.max_entries {
            if let Some((expired, inserted_at)) = self.order.pop_front()
                && self.entries.get(&expired) == Some(&inserted_at)
            {
                self.entries.remove(&expired);
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> bool {
        self.prune();
        self.entries.remove(key).is_some()
    }

    fn prune(&mut self) {
        let now = Instant::now();
        while let Some((key, inserted_at)) = self.order.front() {
            if now.duration_since(*inserted_at) < self.ttl {
                break;
            }
            let key = key.clone();
            let inserted_at = *inserted_at;
            self.order.pop_front();
            if self.entries.get(&key) == Some(&inserted_at) {
                self.entries.remove(&key);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("Starting agent D-Bus service...");

    let conn = connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(ROOT_PATH, zbus::fdo::ObjectManager)?
        .build()
        .await?;

    info!(unique_name = ?conn.unique_name(), "D-Bus connection established");

    let socket_path = socket_path();

    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    info!(path = %socket_path.display(), "Unix socket listening");

    let ended: EndedSessions = Arc::new(Mutex::new(ExpiringSessionSet::new(
        1024,
        Duration::from_secs(10 * 60),
    )));
    let codex_session_parents: CodexSessionParents = Arc::new(Mutex::new(HashMap::new()));
    hooks::start_codex_compact_watcher(conn.clone());
    hooks::start_codex_parent_watcher(
        conn.clone(),
        Arc::clone(&ended),
        Arc::clone(&codex_session_parents),
    );

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let conn = conn.clone();
                let ended = Arc::clone(&ended);
                let codex_session_parents = Arc::clone(&codex_session_parents);
                tokio::spawn(async move {
                    hooks::handle_hook_connection(stream, conn, ended, codex_session_parents).await;
                });
            }
            Err(e) => info!("Socket accept error: {}", e),
        }
    }
}
