use std::path::PathBuf;

pub const BUS_NAME: &str = "io.github.AgentDBus";
pub const ROOT_PATH: &str = "/io/github/AgentDBus";
pub const SESSION_INTERFACE: &str = "io.github.AgentDBus1.Session";
/// Well-known name an approval UI must own to receive blocking requests.
///
/// Without an owner, `agent-dbus` leaves approval prompts to the agent's own
/// native UI instead of holding a blocking hook open for a client that will
/// never answer it.
pub const APPROVAL_UI_BUS_NAME: &str = "io.github.AgentDBus.ApprovalUI";
pub const SOCKET_NAME: &str = "agent-dbus.sock";

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(SOCKET_NAME)
}
