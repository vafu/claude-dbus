use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct LocusContext {
    pub app_instance_id: Option<String>,
    pub window_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RawHook {
    pub agent: String,
    pub event: String,
    pub session_id: String,
    pub data: serde_json::Value,
    pub parent_pid: Option<u32>,
    pub app_instance_id: Option<String>,
    pub window_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NormalizedEvent {
    pub session_id: String,
    pub action: SessionAction,
    pub reply: Option<BlockingReplyKind>,
}

#[derive(Clone, Debug)]
pub enum SessionAction {
    CreateOrUpdate(SessionPatch),
    Remove,
    Notify(String),
    None,
}

#[derive(Clone, Debug, Default)]
pub struct SessionPatch {
    pub state: Option<String>,
    pub task_complete: Option<bool>,
    pub requires_attention: Option<bool>,
    pub model_name: Option<String>,
    pub cwd: Option<String>,
    pub session_title: Option<String>,
    pub metadata: SessionMetadata,
    pub usage_metrics: Option<UsageMetrics>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionMetadata {
    pub app_instance_id: Option<String>,
    pub window_id: Option<String>,
    pub is_subagent: bool,
    pub parent_session_id: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UsageMetrics {
    pub context_pct: Option<f64>,
    pub five_hour: Option<(f64, u64)>,
    pub seven_day: Option<(f64, u64)>,
}

#[derive(Clone, Debug)]
pub enum BlockingReplyKind {
    Permission,
    Elicitation,
}

#[derive(Clone, Debug)]
pub struct PermissionRequestView {
    pub prompt: String,
    pub detail_kind: String,
    pub detail_text: String,
    pub options: Vec<String>,
    pub option_descriptions: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ElicitationRequest {
    pub prompt: String,
    pub detail_kind: String,
    pub detail_text: String,
    pub options: Vec<String>,
    pub option_descriptions: Vec<String>,
}

pub trait ProviderTask: Send + Sync {
    fn start(&self, ctx: ProviderRuntime);
}

#[derive(Clone)]
pub struct ProviderRuntime;

pub trait AgentProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn aliases(&self) -> &'static [&'static str];

    fn normalize_event(&self, hook: &RawHook) -> Option<NormalizedEvent>;
    fn session_metadata(&self, hook: &RawHook) -> SessionMetadata;
    fn usage_metrics(&self, hook: &RawHook) -> Option<UsageMetrics>;
    fn title(&self, hook: &RawHook) -> Option<String>;
    fn subagent(&self, hook: &RawHook) -> Option<SessionMetadata>;

    fn permission_request(&self, hook: &RawHook) -> Option<PermissionRequestView>;
    fn permission_response(&self, hook: &RawHook, answer: &str) -> Option<String>;
    fn should_defer_permission(&self, _hook: &RawHook) -> bool {
        false
    }

    fn empty_success_response(&self) -> Option<&'static str> {
        None
    }

    fn background_tasks(&self) -> Vec<Box<dyn ProviderTask>> {
        Vec::new()
    }
}

pub trait PermissionFormatter {
    fn view(&self, hook: &RawHook) -> PermissionRequestView;
    fn response(&self, hook: &RawHook, answer: &str) -> Option<String>;
}

pub trait ProviderArtifacts {
    fn session_file(&self, session_id: &str) -> Option<PathBuf>;
    fn latest_metrics(&self, session_id: &str) -> Option<UsageMetrics>;
    fn latest_title(&self, session_id: &str) -> Option<String>;
}
