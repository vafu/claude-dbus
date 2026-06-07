use std::path::PathBuf;

pub const BUS_NAME: &str = "io.github.AgentDBus";
pub const ROOT_PATH: &str = "/io/github/AgentDBus";
pub const SESSION_INTERFACE: &str = "io.github.AgentDBus1.Session";
pub const SOCKET_NAME: &str = "agent-dbus.sock";

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(SOCKET_NAME)
}
