use zbus::zvariant::ObjectPath;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let agent = args.next().unwrap_or_default();
    let session_id = args.next().unwrap_or_default();
    let answer = args.collect::<Vec<_>>().join(" ");

    if agent.is_empty() || session_id.is_empty() || answer.is_empty() {
        eprintln!("Usage: agent-respond <AgentName> <SessionId> <Answer>");
        std::process::exit(1);
    }

    let path = session_path(&agent, &session_id);
    let conn = zbus::Connection::session().await?;
    conn.call_method(
        Some("io.github.AgentDBus"),
        path,
        Some("io.github.AgentDBus1.Session"),
        "RespondToElicitation",
        &(answer.as_str()),
    )
    .await?;

    Ok(())
}

fn session_path(agent_name: &str, session_id: &str) -> ObjectPath<'static> {
    let safe_agent = safe_path_segment(agent_name);
    let safe_id = safe_path_segment(session_id);
    ObjectPath::try_from(format!(
        "/io/github/AgentDBus/sessions/{}/{}",
        safe_agent, safe_id
    ))
    .unwrap()
}

fn safe_path_segment(value: &str) -> String {
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
