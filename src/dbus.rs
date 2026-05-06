use std::collections::VecDeque;

use tracing::{debug, warn};
use zbus::{interface, object_server::SignalEmitter};

use crate::types::SessionState;
use agent_dbus::path::session_path;

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
    pub pending_requests: VecDeque<PendingRequest>,
}

#[derive(PartialEq)]
struct SessionSnapshot {
    agent_name: String,
    state: SessionState,
    task_complete: bool,
    requires_attention: bool,
    context_pct: f64,
    model_name: String,
    cwd: String,
    cost_usd: f64,
    five_hour_usage_pct: f64,
    five_hour_resets_at: u64,
    seven_day_usage_pct: f64,
    seven_day_resets_at: u64,
    pending_prompt: String,
    pending_options: Vec<String>,
    pending_count: usize,
    pending_request_ids: Vec<String>,
    pending_prompts: Vec<String>,
    pending_options_list: Vec<Vec<String>>,
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
            pending_requests: VecDeque::new(),
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
            .front()
            .map(|request| request.prompt.as_str())
            .unwrap_or("")
    }

    pub fn pending_options_value(&self) -> Vec<&str> {
        self.pending_requests
            .front()
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

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            agent_name: self.agent_name.clone(),
            state: self.state.clone(),
            task_complete: self.task_complete,
            requires_attention: self.requires_attention,
            context_pct: self.context_pct,
            model_name: self.model_name.clone(),
            cwd: self.cwd.clone(),
            cost_usd: self.cost_usd,
            five_hour_usage_pct: self.five_hour_usage_pct,
            five_hour_resets_at: self.five_hour_resets_at,
            seven_day_usage_pct: self.seven_day_usage_pct,
            seven_day_resets_at: self.seven_day_resets_at,
            pending_prompt: self.pending_prompt_value().to_string(),
            pending_options: self
                .pending_options_value()
                .into_iter()
                .map(str::to_string)
                .collect(),
            pending_count: self.pending_requests.len(),
            pending_request_ids: self
                .pending_request_ids_value()
                .into_iter()
                .map(str::to_string)
                .collect(),
            pending_prompts: self
                .pending_prompts_value()
                .into_iter()
                .map(str::to_string)
                .collect(),
            pending_options_list: self
                .pending_options_list_value()
                .into_iter()
                .map(|options| options.into_iter().map(str::to_string).collect())
                .collect(),
        }
    }

    fn take_pending_response(
        &mut self,
        request_id: Option<&str>,
    ) -> Option<tokio::sync::oneshot::Sender<String>> {
        let request = match request_id {
            Some(request_id) => {
                let index = self
                    .pending_requests
                    .iter()
                    .position(|request| request.id == request_id)?;
                self.pending_requests.remove(index)?
            }
            None => self.pending_requests.pop_front()?,
        };
        self.requires_attention = !self.pending_requests.is_empty();
        Some(request.tx)
    }

    pub fn push_pending_request(&mut self, request: PendingRequest) {
        self.pending_requests.push_back(request);
        self.requires_attention = true;
    }

    pub fn remove_pending_request(&mut self, request_id: &str) -> bool {
        let Some(index) = self
            .pending_requests
            .iter()
            .position(|request| request.id == request_id)
        else {
            return false;
        };
        self.pending_requests.remove(index);
        self.requires_attention = !self.pending_requests.is_empty();
        true
    }

    pub fn cancel_pending_requests(&mut self) -> usize {
        let requests = std::mem::take(&mut self.pending_requests);
        self.requires_attention = false;
        let count = requests.len();
        for request in requests {
            let _ = request.tx.send(String::new());
        }
        count
    }

    pub async fn emit_pending_changed(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        self.requires_attention_changed(emitter).await?;
        self.pending_prompt_changed(emitter).await?;
        self.pending_options_changed(emitter).await?;
        self.pending_count_changed(emitter).await?;
        self.pending_request_ids_changed(emitter).await?;
        self.pending_prompts_changed(emitter).await?;
        self.pending_options_list_changed(emitter).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn pending_request(id: &str) -> (PendingRequest, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        (
            PendingRequest {
                id: id.to_string(),
                prompt: format!("prompt {id}"),
                options: vec!["Allow".to_string(), "Deny".to_string()],
                tx,
            },
            rx,
        )
    }

    #[test]
    fn pending_requests_are_fifo_for_legacy_response() {
        let mut session = SessionObject::new("codex");
        let (first, _first_rx) = pending_request("req-1");
        let (second, _second_rx) = pending_request("req-2");
        session.push_pending_request(first);
        session.push_pending_request(second);

        let tx = session.take_pending_response(None);

        assert!(tx.is_some());
        assert_eq!(session.pending_request_ids_value(), vec!["req-2"]);
        assert!(session.requires_attention);
    }

    #[test]
    fn pending_requests_can_be_removed_by_id() {
        let mut session = SessionObject::new("codex");
        let (first, _first_rx) = pending_request("req-1");
        let (second, _second_rx) = pending_request("req-2");
        session.push_pending_request(first);
        session.push_pending_request(second);

        let tx = session.take_pending_response(Some("req-2"));

        assert!(tx.is_some());
        assert_eq!(session.pending_request_ids_value(), vec!["req-1"]);
        assert!(session.requires_attention);
    }

    #[tokio::test]
    async fn cancel_pending_requests_answers_empty_and_clears_attention() {
        let mut session = SessionObject::new("codex");
        let (request, rx) = pending_request("req-1");
        session.push_pending_request(request);

        assert_eq!(session.cancel_pending_requests(), 1);

        assert_eq!(rx.await.unwrap(), "");
        assert!(session.pending_requests.is_empty());
        assert!(!session.requires_attention);
    }
}

async fn emit_changed_properties(
    iface: &SessionObject,
    emitter: &SignalEmitter<'_>,
    before: &SessionSnapshot,
    after: &SessionSnapshot,
) -> zbus::Result<()> {
    if before.agent_name != after.agent_name {
        iface.agent_name_changed(emitter).await?;
    }
    if before.state != after.state {
        iface.state_changed(emitter).await?;
    }
    if before.task_complete != after.task_complete {
        iface.task_complete_changed(emitter).await?;
    }
    if before.requires_attention != after.requires_attention {
        iface.requires_attention_changed(emitter).await?;
    }
    if before.context_pct != after.context_pct {
        iface.context_pct_changed(emitter).await?;
    }
    if before.model_name != after.model_name {
        iface.model_name_changed(emitter).await?;
    }
    if before.cwd != after.cwd {
        iface.cwd_changed(emitter).await?;
    }
    if before.cost_usd != after.cost_usd {
        iface.cost_usd_changed(emitter).await?;
    }
    if before.five_hour_usage_pct != after.five_hour_usage_pct {
        iface.five_hour_usage_pct_changed(emitter).await?;
    }
    if before.five_hour_resets_at != after.five_hour_resets_at {
        iface.five_hour_resets_at_changed(emitter).await?;
    }
    if before.seven_day_usage_pct != after.seven_day_usage_pct {
        iface.seven_day_usage_pct_changed(emitter).await?;
    }
    if before.seven_day_resets_at != after.seven_day_resets_at {
        iface.seven_day_resets_at_changed(emitter).await?;
    }
    if before.pending_prompt != after.pending_prompt {
        iface.pending_prompt_changed(emitter).await?;
    }
    if before.pending_options != after.pending_options {
        iface.pending_options_changed(emitter).await?;
    }
    if before.pending_count != after.pending_count {
        iface.pending_count_changed(emitter).await?;
    }
    if before.pending_request_ids != after.pending_request_ids {
        iface.pending_request_ids_changed(emitter).await?;
    }
    if before.pending_prompts != after.pending_prompts {
        iface.pending_prompts_changed(emitter).await?;
    }
    if before.pending_options_list != after.pending_options_list {
        iface.pending_options_list_changed(emitter).await?;
    }
    Ok(())
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
                if let Err(err) = self.emit_pending_changed(&emitter).await {
                    warn!(%err, "failed to emit pending response properties");
                }
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
                if let Err(err) = self.emit_pending_changed(&emitter).await {
                    warn!(%err, "failed to emit pending response properties");
                }
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
    if let Err(err) = conn
        .object_server()
        .at(&path, SessionObject::new(agent_name))
        .await
    {
        warn!(%err, %session_id, "failed to create session object");
    }
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
        .map_err(|err| {
            warn!(%err, %session_id, "failed to ensure session object");
            err
        })?;
    if created {
        debug!(session_id = %session_id, "auto-created session object");
    }
    let iface_ref = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await?;
    let (before, after) = {
        let mut iface = iface_ref.get_mut().await;
        let before = iface.snapshot();
        iface.agent_name = agent_name.to_string();
        f(&mut iface);
        let after = iface.snapshot();
        (before, after)
    };
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
    emit_changed_properties(&iface, emitter, &before, &after).await?;
    Ok(())
}
