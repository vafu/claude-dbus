use tokio::net::UnixListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::connection;

mod dbus;
mod hooks;
mod types;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("Starting Claude D-Bus Service...");

    let conn = connection::Builder::session()?
        .name("com.anthropic.ClaudeCode")?
        .serve_at("/com/anthropic/ClaudeCode", zbus::fdo::ObjectManager)?
        .build()
        .await?;

    info!(unique_name = ?conn.unique_name(), "D-Bus connection established");

    let socket_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{}/claude-code.sock", d))
        .unwrap_or_else(|_| "/tmp/claude-code.sock".to_string());

    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    info!(path = %socket_path, "Unix socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let conn = conn.clone();
                tokio::spawn(async move {
                    hooks::handle_hook_connection(stream, conn).await;
                });
            }
            Err(e) => info!("Socket accept error: {}", e),
        }
    }
}
