use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use agent_dbus::constants::socket_path;
use agent_dbus::path::{agent_session_node_key, safe_path_segment};

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
        "hook_pid": std::process::id(),
        "parent_pid": owning_process_pid(),
    });
    let msg_bytes = serde_json::to_vec(&msg)?;

    let socket_path = socket_path();

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

fn owning_process_pid() -> Option<u32> {
    let direct_parent = process_parent_pid(std::process::id())?;
    let mut pid = direct_parent;

    for _ in 0..32 {
        if process_name(pid)
            .as_deref()
            .is_some_and(|name| name.contains("codex"))
        {
            return Some(pid);
        }
        let Some(parent_pid) = process_parent_pid(pid) else {
            break;
        };
        pid = parent_pid;
    }

    Some(direct_parent)
}

fn process_parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat).map(|(_, parent_pid)| parent_pid)
}

fn process_name(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat).map(|(name, _)| name)
}

fn parse_stat(stat: &str) -> Option<(String, u32)> {
    let open_paren = stat.find('(')?;
    let close_paren = stat.rfind(") ")?;
    let name = stat[open_paren + 1..close_paren].to_string();
    let mut fields = stat[close_paren + 2..].split_whitespace();
    fields.next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    Some((name, parent_pid))
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

    let key = agent_session_node_key(agent, session_id);
    let remove = event == "SessionEnd";

    let app_instance = std::env::var("LOCUS_APP_INSTANCE").unwrap_or_default();
    if !app_instance.is_empty() {
        update_locus_agent_session_link(&locus, &key, &app_instance, None, remove).await;
        return;
    }

    let window_id = std::env::var("AGENT_DBUS_WINDOW_ID").unwrap_or_default();
    if !window_id.is_empty() {
        let app_instance = format!("app-instance:{key}");
        update_locus_agent_session_link(&locus, &key, &app_instance, Some((agent, &window_id)), remove)
            .await;
    }
}

async fn update_locus_agent_session_link(
    locus: &locus::Client<'_>,
    key: &str,
    app_instance: &str,
    fallback_window: Option<(&str, &str)>,
    remove: bool,
) {
    let target = format!("agent-session:{key}");
    if remove {
        let _ = locus
            .remove_link(app_instance, "agent-session", &target)
            .await;
        if let Some((_, window_id)) = fallback_window {
            let window = format!("window:{window_id}");
            let _ = locus
                .remove_link(&window, "app-instance", app_instance)
                .await;
        }
    } else {
        if let Some((agent, window_id)) = fallback_window {
            let window = format!("window:{window_id}");
            let _ = locus.set_property(app_instance, "kind", "app-instance").await;
            let _ = locus.set_property(app_instance, "name", agent).await;
            let _ = locus
                .set_property(app_instance, "icon", &safe_path_segment(agent))
                .await;
            let _ = locus.set_link(&window, "app-instance", app_instance).await;
        }
        let _ = locus.set_property(&target, "kind", "agent-session").await;
        let _ = locus.set_link(app_instance, "agent-session", &target).await;
    }
}
