pub fn is_gemini_agent(agent_name: &str) -> bool {
    matches!(agent_name, "gemini" | "gemini-cli")
}

pub fn is_opencode_agent(agent_name: &str) -> bool {
    matches!(agent_name, "opencode" | "opencode-cli")
}
