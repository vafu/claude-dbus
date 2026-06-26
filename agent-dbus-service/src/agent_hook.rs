use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use agent_dbus_core::agent::is_gemini_agent;
use agent_dbus_core::constants::socket_path;

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

    let selected_window = locusfs_selected_window_path();
    let app_instance_id = std::env::var("LOCUS_APP_INSTANCE")
        .ok()
        .and_then(non_empty_string)
        .or_else(|| locusfs_selected_app_instance_id(selected_window.as_deref()))
        .unwrap_or_default();
    let window_id = std::env::var("AGENT_DBUS_WINDOW")
        .or_else(|_| std::env::var("AGENT_DBUS_WINDOW_ID"))
        .ok()
        .and_then(non_empty_string)
        .or_else(|| locusfs_selected_window_id(selected_window.as_deref()))
        .unwrap_or_default();

    let msg = serde_json::json!({
        "agent": agent,
        "event": event,
        "data": data,
        "hook_pid": std::process::id(),
        "parent_pid": owning_process_pid(),
        "app_instance_id": app_instance_id,
        "window_id": window_id,
    });
    let msg_bytes = serde_json::to_vec(&msg)?;

    let socket_path = socket_path();

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(_) => {
            print_empty_response_if_needed(&agent);
            return Ok(());
        }
    };

    stream.write_all(&msg_bytes)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    if !response.is_empty() {
        print!("{}", response);
    } else {
        print_empty_response_if_needed(&agent);
    }

    Ok(())
}

fn print_empty_response_if_needed(agent: &str) {
    if is_gemini_agent(agent) {
        print!("{{}}");
    }
}

fn non_empty_string(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn locusfs_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("LOCUS_ROOT") {
        return Some(PathBuf::from(root));
    }
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(runtime_dir).join("locusfs"));
    }
    std::env::var_os("UID")
        .map(PathBuf::from)
        .map(|uid| Path::new("/run/user").join(uid).join("locusfs"))
}

fn locusfs_selected_window_path() -> Option<PathBuf> {
    std::fs::canonicalize(locusfs_root()?.join("context/selected/window")).ok()
}

fn locusfs_selected_app_instance_id(selected_window: Option<&Path>) -> Option<String> {
    let app_instance = std::fs::canonicalize(selected_window?.join("app-instance")).ok()?;
    locusfs_node_ref("app-instance", &app_instance)
}

fn locusfs_selected_window_id(selected_window: Option<&Path>) -> Option<String> {
    selected_window?.file_name()?.to_str().map(str::to_owned)
}

fn locusfs_node_ref(kind: &str, path: &Path) -> Option<String> {
    let local_id = path.file_name()?.to_str()?;
    Some(format!("{kind}:{local_id}"))
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{locusfs_node_ref, locusfs_selected_window_id, non_empty_string, parse_stat};

    #[test]
    fn non_empty_string_trims_and_filters() {
        assert_eq!(
            non_empty_string("  window:7  ".to_string()),
            Some("window:7".to_string())
        );
        assert_eq!(non_empty_string("  ".to_string()), None);
    }

    #[test]
    fn locusfs_node_ref_uses_basename() {
        assert_eq!(
            locusfs_node_ref(
                "app-instance",
                Path::new("/run/user/1000/locusfs/app-instance/codex_1")
            ),
            Some("app-instance:codex_1".to_string())
        );
    }

    #[test]
    fn selected_window_id_uses_basename() {
        assert_eq!(
            locusfs_selected_window_id(Some(Path::new("/run/user/1000/locusfs/window/42"))),
            Some("42".to_string())
        );
    }

    #[test]
    fn parse_stat_handles_command_names_with_spaces() {
        assert_eq!(
            parse_stat("123 (codex helper) S 42 1 1 0"),
            Some(("codex helper".to_string(), 42))
        );
    }
}
