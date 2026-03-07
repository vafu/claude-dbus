use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::connection;

mod dbus;
mod hooks;
mod types;

use dbus::ClaudeStatus;
use types::{ElicitationTxs, Sessions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("Starting Claude D-Bus Service...");

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let elicitation_txs: ElicitationTxs = Arc::new(Mutex::new(HashMap::new()));

    let server = ClaudeStatus {
        sessions: Arc::clone(&sessions),
        elicitation_txs: Arc::clone(&elicitation_txs),
    };

    let conn = connection::Builder::session()?
        .name("com.anthropic.ClaudeCode")?
        .serve_at("/com/anthropic/ClaudeCode", server)?
        .build()
        .await?;

    info!(unique_name = ?conn.unique_name(), "D-Bus connection established");

    let socket_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{}/claude-code.sock", d))
        .unwrap_or_else(|_| "/tmp/claude-code.sock".to_string());

    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    info!(path = %socket_path, "Unix socket listening");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let sessions = Arc::clone(&sessions);
                    let elicitation_txs = Arc::clone(&elicitation_txs);
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        hooks::handle_hook_connection(stream, sessions, elicitation_txs, conn)
                            .await;
                    });
                }
                Err(e) => info!("Socket accept error: {}", e),
            }
        }
    });

    std::future::pending::<()>().await;
    Ok(())
}
