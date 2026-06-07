#[derive(Clone, Default, PartialEq)]
pub enum SessionState {
    #[default]
    NoSession,
    Idle,
    Thinking,
    ToolUse,
    Compacting,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoSession => "no-session",
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::ToolUse => "tool-use",
            Self::Compacting => "compacting",
        }
    }
}
