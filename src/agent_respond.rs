use agent_dbus::constants::{BUS_NAME, SESSION_INTERFACE};
use agent_dbus::path::session_path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let agent = args.next().unwrap_or_default();
    let session_id = args.next().unwrap_or_default();
    let mut request_id = String::new();
    let mut answer_parts = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--request-id" {
            request_id = args.next().unwrap_or_default();
        } else {
            answer_parts.push(arg);
        }
    }
    let answer = answer_parts.join(" ");

    if agent.is_empty() || session_id.is_empty() || answer.is_empty() {
        eprintln!(
            "Usage: agent-respond <AgentName> <SessionId> [--request-id <RequestId>] <Answer>"
        );
        std::process::exit(1);
    }

    let path = session_path(&agent, &session_id);
    let conn = zbus::Connection::session().await?;
    if request_id.is_empty() {
        conn.call_method(
            Some(BUS_NAME),
            path,
            Some(SESSION_INTERFACE),
            "RespondToElicitation",
            &(answer.as_str()),
        )
        .await?;
    } else {
        conn.call_method(
            Some(BUS_NAME),
            path,
            Some(SESSION_INTERFACE),
            "RespondToElicitationById",
            &(request_id.as_str(), answer.as_str()),
        )
        .await?;
    }

    Ok(())
}
