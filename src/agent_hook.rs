use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (agent, event) = parse_args();
    if event.is_empty() {
        eprintln!("Usage: agent-hook [AgentName] <EventName>");
        std::process::exit(1);
    }

    let mut stdin_data = String::new();
    std::io::stdin().read_to_string(&mut stdin_data)?;

    let data: serde_json::Value =
        serde_json::from_str(stdin_data.trim()).unwrap_or(serde_json::Value::Null);
    record_session_window_if_present(&agent, &event, &data);

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

fn record_session_window_if_present(agent: &str, event: &str, data: &serde_json::Value) {
    let session_id = data["session_id"].as_str().unwrap_or("");
    let window_id = std::env::var("AGENT_DBUS_WINDOW_ID").unwrap_or_default();
    if session_id.is_empty() || window_id.is_empty() {
        return;
    }

    record_session_window(agent, session_id, &window_id, event == "SessionEnd");
}

fn record_session_window(agent: &str, session_id: &str, window_id: &str, remove: bool) {
    let key = format!("{}/{}", safe_segment(agent), safe_segment(session_id));
    update_mapping_file(&key, window_id, remove);
    notify_ags(&key, window_id, remove);
}

fn update_mapping_file(key: &str, window_id: &str, remove: bool) {
    let path = mapping_path();
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);

    let mut mappings = read_mappings(&path);
    match remove {
        true => {
            mappings.remove(key);
        }
        false => {
            mappings.insert(key.to_string(), window_id.to_string());
        }
    }

    let Ok(contents) = serde_json::to_string(&mappings) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, contents).is_ok() {
        let _ = std::fs::rename(tmp, &path);
    }
}

fn read_mappings(path: &PathBuf) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn notify_ags(key: &str, window_id: &str, remove: bool) {
    let mut command = vec!["request", "agent-session-window", "--session-id", key];
    if remove {
        command.extend(["--remove", "true"]);
    } else {
        command.extend(["--window-id", window_id]);
    }
    let _ = Command::new("ags")
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn mapping_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("agent-dbus")
        .join("session-windows.json")
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
