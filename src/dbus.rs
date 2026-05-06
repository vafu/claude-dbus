use tracing::debug;
use zbus::{interface, object_server::SignalEmitter, zvariant::ObjectPath};

use crate::types::SessionState;

pub struct PendingRequest {
    pub id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub tx: tokio::sync::oneshot::Sender<String>,
}

pub struct SessionObject {
    pub agent_name: String,
    pub state: SessionState,
    pub task_complete: bool,
    pub requires_attention: bool,
    pub context_pct: f64,
    pub model_name: String,
    pub cwd: String,
    pub cost_usd: f64,
    pub five_hour_usage_pct: f64,
    pub five_hour_resets_at: u64,
    pub seven_day_usage_pct: f64,
    pub seven_day_resets_at: u64,
    pub pending_requests: Vec<PendingRequest>,
}

impl Default for SessionObject {
    fn default() -> Self {
        Self {
            agent_name: String::new(),
            state: SessionState::NoSession,
            task_complete: false,
            requires_attention: false,
            context_pct: 0.0,
            model_name: String::new(),
            cwd: String::new(),
            cost_usd: 0.0,
            five_hour_usage_pct: 0.0,
            five_hour_resets_at: 0,
            seven_day_usage_pct: 0.0,
            seven_day_resets_at: 0,
            pending_requests: Vec::new(),
        }
    }
}

impl SessionObject {
    pub fn new(agent_name: &str) -> Self {
        Self {
            agent_name: agent_name.to_string(),
            ..Self::default()
        }
    }

    pub fn pending_prompt_value(&self) -> &str {
        self.pending_requests
            .first()
            .map(|request| request.prompt.as_str())
            .unwrap_or("")
    }

    pub fn pending_options_value(&self) -> Vec<&str> {
        self.pending_requests
            .first()
            .map(|request| request.options.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn pending_request_ids_value(&self) -> Vec<&str> {
        self.pending_requests
            .iter()
            .map(|request| request.id.as_str())
            .collect()
    }

    pub fn pending_prompts_value(&self) -> Vec<&str> {
        self.pending_requests
            .iter()
            .map(|request| request.prompt.as_str())
            .collect()
    }

    pub fn pending_options_list_value(&self) -> Vec<Vec<&str>> {
        self.pending_requests
            .iter()
            .map(|request| request.options.iter().map(String::as_str).collect())
            .collect()
    }

    fn take_pending_response(
        &mut self,
        request_id: Option<&str>,
    ) -> Option<tokio::sync::oneshot::Sender<String>> {
        let index = match request_id {
            Some(request_id) => self
                .pending_requests
                .iter()
                .position(|request| request.id == request_id)?,
            None => {
                if self.pending_requests.is_empty() {
                    return None;
                }
                0
            }
        };
        let request = self.pending_requests.remove(index);
        self.requires_attention = !self.pending_requests.is_empty();
        Some(request.tx)
    }
}

#[interface(name = "io.github.AgentDBus1.Session")]
impl SessionObject {
    #[zbus(property)]
    fn agent_name(&self) -> &str {
        &self.agent_name
    }

    #[zbus(property)]
    fn state(&self) -> &str {
        self.state.as_str()
    }

    #[zbus(property)]
    fn task_complete(&self) -> bool {
        self.task_complete
    }

    #[zbus(property)]
    fn requires_attention(&self) -> bool {
        self.requires_attention
    }

    #[zbus(property)]
    fn context_pct(&self) -> f64 {
        self.context_pct
    }

    #[zbus(property)]
    fn model_name(&self) -> &str {
        &self.model_name
    }

    #[zbus(property)]
    fn cwd(&self) -> &str {
        &self.cwd
    }

    #[zbus(property)]
    fn cost_usd(&self) -> f64 {
        self.cost_usd
    }

    #[zbus(property)]
    fn five_hour_usage_pct(&self) -> f64 {
        self.five_hour_usage_pct
    }

    #[zbus(property)]
    fn five_hour_resets_at(&self) -> u64 {
        self.five_hour_resets_at
    }

    #[zbus(property)]
    fn seven_day_usage_pct(&self) -> f64 {
        self.seven_day_usage_pct
    }

    #[zbus(property)]
    fn seven_day_resets_at(&self) -> u64 {
        self.seven_day_resets_at
    }

    #[zbus(property)]
    fn pending_prompt(&self) -> &str {
        self.pending_prompt_value()
    }

    #[zbus(property)]
    fn pending_options(&self) -> Vec<&str> {
        self.pending_options_value()
    }

    #[zbus(property)]
    fn pending_count(&self) -> u32 {
        self.pending_requests.len() as u32
    }

    #[zbus(property)]
    fn pending_request_ids(&self) -> Vec<&str> {
        self.pending_request_ids_value()
    }

    #[zbus(property)]
    fn pending_prompts(&self) -> Vec<&str> {
        self.pending_prompts_value()
    }

    #[zbus(property)]
    fn pending_options_list(&self) -> Vec<Vec<&str>> {
        self.pending_options_list_value()
    }

    async fn respond_to_elicitation(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        answer: &str,
    ) {
        if !answer.is_empty() {
            if let Some(tx) = self.take_pending_response(None) {
                let _ = tx.send(answer.to_string());
                let _ = self.requires_attention_changed(&emitter).await;
                let _ = self.pending_prompt_changed(&emitter).await;
                let _ = self.pending_options_changed(&emitter).await;
                let _ = self.pending_count_changed(&emitter).await;
                let _ = self.pending_request_ids_changed(&emitter).await;
                let _ = self.pending_prompts_changed(&emitter).await;
                let _ = self.pending_options_list_changed(&emitter).await;
            }
        }
    }

    async fn respond_to_elicitation_by_id(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        request_id: &str,
        answer: &str,
    ) {
        if !request_id.is_empty() && !answer.is_empty() {
            if let Some(tx) = self.take_pending_response(Some(request_id)) {
                let _ = tx.send(answer.to_string());
                let _ = self.requires_attention_changed(&emitter).await;
                let _ = self.pending_prompt_changed(&emitter).await;
                let _ = self.pending_options_changed(&emitter).await;
                let _ = self.pending_count_changed(&emitter).await;
                let _ = self.pending_request_ids_changed(&emitter).await;
                let _ = self.pending_prompts_changed(&emitter).await;
                let _ = self.pending_options_list_changed(&emitter).await;
            }
        }
    }

    #[zbus(signal)]
    async fn elicitation_requested(
        emitter: &SignalEmitter<'_>,
        prompt: &str,
        options: &[&str],
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn elicitation_requested_with_id(
        emitter: &SignalEmitter<'_>,
        request_id: &str,
        prompt: &str,
        options: &[&str],
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;
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

pub fn session_path(agent_name: &str, session_id: &str) -> ObjectPath<'static> {
    let safe_agent = safe_path_segment(agent_name);
    let safe_id = safe_path_segment(session_id);
    ObjectPath::try_from(format!(
        "/io/github/AgentDBus/sessions/{}/{}",
        safe_agent, safe_id
    ))
    .unwrap()
}

pub async fn emit_notification(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()> {
    SessionObject::notification(emitter, message).await
}

pub async fn emit_elicitation(
    emitter: &SignalEmitter<'_>,
    prompt: &str,
    options: &[&str],
) -> zbus::Result<()> {
    SessionObject::elicitation_requested(emitter, prompt, options).await
}

pub async fn emit_elicitation_with_id(
    emitter: &SignalEmitter<'_>,
    request_id: &str,
    prompt: &str,
    options: &[&str],
) -> zbus::Result<()> {
    SessionObject::elicitation_requested_with_id(emitter, request_id, prompt, options).await
}

pub async fn create_session(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
) -> zbus::Result<()> {
    let path = session_path(agent_name, session_id);
    let _ = conn
        .object_server()
        .at(&path, SessionObject::new(agent_name))
        .await;
    Ok(())
}

pub async fn update_session(
    conn: &zbus::Connection,
    agent_name: &str,
    session_id: &str,
    f: impl FnOnce(&mut SessionObject),
) -> zbus::Result<()> {
    let path = session_path(agent_name, session_id);
    let created = conn
        .object_server()
        .at(&path, SessionObject::new(agent_name))
        .await
        .unwrap_or(false);
    if created {
        debug!(session_id = %session_id, "auto-created session object");
    }
    let iface_ref = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await?;
    {
        let mut iface = iface_ref.get_mut().await;
        iface.agent_name = agent_name.to_string();
        f(&mut iface);
    }
    let emitter = iface_ref.signal_emitter();
    let iface = iface_ref.get().await;
    debug!(
        session_id = %session_id,
        state = %iface.state.as_str(),
        task_complete = %iface.task_complete,
        requires_attention = %iface.requires_attention,
        context_pct = %iface.context_pct,
        model = %iface.model_name,
        "session updated"
    );
    iface.agent_name_changed(emitter).await?;
    iface.state_changed(emitter).await?;
    iface.task_complete_changed(emitter).await?;
    iface.requires_attention_changed(emitter).await?;
    iface.context_pct_changed(emitter).await?;
    iface.model_name_changed(emitter).await?;
    iface.cwd_changed(emitter).await?;
    iface.cost_usd_changed(emitter).await?;
    iface.five_hour_usage_pct_changed(emitter).await?;
    iface.five_hour_resets_at_changed(emitter).await?;
    iface.seven_day_usage_pct_changed(emitter).await?;
    iface.seven_day_resets_at_changed(emitter).await?;
    iface.pending_prompt_changed(emitter).await?;
    iface.pending_options_changed(emitter).await?;
    iface.pending_count_changed(emitter).await?;
    iface.pending_request_ids_changed(emitter).await?;
    iface.pending_prompts_changed(emitter).await?;
    iface.pending_options_list_changed(emitter).await?;
    Ok(())
}
