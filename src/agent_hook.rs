use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (agent, event) = parse_args();
    if event.is_empty() {
        eprintln!("Usage: agent-hook [AgentName] <EventName>");
        std::process::exit(1);
    }

    let mut stdin_data = String::new();
    std::io::stdin().read_to_string(&mut stdin_data)?;

    let data: serde_json::Value =
        serde_json::from_str(stdin_data.trim()).unwrap_or(serde_json::Value::Null);
    record_session_links_if_present(&agent, &event, &data).await;

    let msg = serde_json::json!({
        "agent": agent,
        "event": event,
        "data": data,
    });
    let msg_bytes = serde_json::to_vec(&msg)?;

    let socket_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{}/agent-dbus.sock", d))
        .unwrap_or_else(|_| "/tmp/agent-dbus.sock".to_string());

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(_) => return Ok(()), // Service unavailable - fall through
    };

    stream.write_all(&msg_bytes)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    if !response.is_empty() {
        print!("{}", response);
    }

    Ok(())
}

fn parse_args() -> (String, String) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [event] => (
            std::env::var("AGENT_DBUS_AGENT").unwrap_or_else(|_| "agent".to_string()),
            event.clone(),
        ),
        [agent, event, ..] => (agent.clone(), event.clone()),
        [] => (String::new(), String::new()),
    }
}

async fn record_session_links_if_present(agent: &str, event: &str, data: &serde_json::Value) {
    let session_id = data["session_id"].as_str().unwrap_or("");
    if session_id.is_empty() {
        return;
    }

    let Ok(connection) = zbus::Connection::session().await else {
        return;
    };
    let Ok(locus) = locus::Client::new(&connection).await else {
        return;
    };

    let key = format!("{}/{}", safe_segment(agent), safe_segment(session_id));
    let remove = event == "SessionEnd";

    let window_id = std::env::var("AGENT_DBUS_WINDOW_ID").unwrap_or_default();
    if !window_id.is_empty() {
        update_locus_window_session_link(&locus, &key, &window_id, remove).await;
    }

    update_locus_project_session_link(&locus, &key, data["cwd"].as_str(), remove).await;
}

async fn update_locus_window_session_link(
    locus: &locus::Client<'_>,
    key: &str,
    window_id: &str,
    remove: bool,
) {
    let source = format!("niri:window:{window_id}");
    let target = format!("agent-session:{key}");
    if remove {
        let _ = locus.remove_link(&source, "agent-session", &target).await;
    } else {
        let _ = locus
            .add_link(&source, "agent-session", &target, false)
            .await;
    }
}

async fn update_locus_project_session_link(
    locus: &locus::Client<'_>,
    key: &str,
    cwd: Option<&str>,
    remove: bool,
) {
    let target = format!("agent-session:{key}");
    if remove {
        remove_locus_project_session_links(locus, &target).await;
        return;
    }

    let Some(project_root) = cwd.and_then(project_root_for_cwd) else {
        return;
    };
    let Some(source) = ensure_locus_project(locus, &project_root).await else {
        return;
    };

    let _ = locus
        .add_link(&source, "agent-session", &target, false)
        .await;
}

async fn remove_locus_project_session_links(locus: &locus::Client<'_>, target: &str) {
    let Ok(sources) = locus.sources(target, "agent-session").await else {
        return;
    };

    for source in sources {
        if source.starts_with("project:") {
            let _ = locus.remove_link(&source, "agent-session", target).await;
        }
    }
}

async fn ensure_locus_project(locus: &locus::Client<'_>, path: &Path) -> Option<String> {
    let path = path.to_str()?;
    locus
        .ensure_project_spec(locus::ProjectSpec::new(path))
        .await
        .ok()
        .filter(|subject| !subject.is_empty())
}

fn project_root_for_cwd(cwd: &str) -> Option<PathBuf> {
    let proj_dir = PathBuf::from(std::env::var_os("HOME")?).join("proj");
    let cwd = PathBuf::from(cwd);
    let relative = cwd.strip_prefix(&proj_dir).ok()?;
    let Some(Component::Normal(project_name)) = relative.components().next() else {
        return None;
    };

    Some(proj_dir.join(project_name))
}

fn safe_segment(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}
