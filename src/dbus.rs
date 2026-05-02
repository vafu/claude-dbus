use tracing::debug;
use zbus::{interface, object_server::SignalEmitter, zvariant::ObjectPath};

use crate::types::SessionState;

pub struct SessionObject {
    pub agent_name: String,
    pub state: SessionState,
    pub task_complete: bool,
    pub requires_attention: bool,
    pub context_pct: f64,
    pub model_name: String,
    pub cwd: String,
    pub cost_usd: f64,
    pub pending_prompt: String,
    pub pending_options: Vec<String>,
    pub elicitation_tx: Option<tokio::sync::oneshot::Sender<String>>,
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
            pending_prompt: String::new(),
            pending_options: Vec::new(),
            elicitation_tx: None,
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
    fn pending_prompt(&self) -> &str {
        &self.pending_prompt
    }

    #[zbus(property)]
    fn pending_options(&self) -> Vec<&str> {
        self.pending_options.iter().map(String::as_str).collect()
    }

    async fn respond_to_elicitation(&mut self, answer: &str) {
        if !answer.is_empty() {
            if let Some(tx) = self.elicitation_tx.take() {
                let _ = tx.send(answer.to_string());
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
    iface.pending_prompt_changed(emitter).await?;
    iface.pending_options_changed(emitter).await?;
    Ok(())
}
