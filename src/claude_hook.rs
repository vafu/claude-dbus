use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event = std::env::args().nth(1).unwrap_or_default();
    if event.is_empty() {
        eprintln!("Usage: claude-hook <EventName>");
        std::process::exit(1);
    }

    let mut stdin_data = String::new();
    std::io::stdin().read_to_string(&mut stdin_data)?;

    let data: serde_json::Value =
        serde_json::from_str(stdin_data.trim()).unwrap_or(serde_json::Value::Null);
    let msg = serde_json::json!({"event": event, "data": data});
    let msg_bytes = serde_json::to_vec(&msg)?;

    let socket_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{}/claude-code.sock", d))
        .unwrap_or_else(|_| "/tmp/claude-code.sock".to_string());

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(_) => return Ok(()), // Service unavailable — fall through
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
