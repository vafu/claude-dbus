use tracing::instrument;
use zbus::{interface, object_server::SignalContext};

use crate::types::{ClaudeData, ElicitationTxs, Sessions};

pub struct ClaudeStatus {
    pub sessions: Sessions,
    pub elicitation_txs: ElicitationTxs,
}

#[interface(name = "com.anthropic.ClaudeCode1")]
impl ClaudeStatus {
    #[instrument(skip(self))]
    pub async fn respond_to_elicitation(
        &self,
        session_id: &str,
        answer: &str,
    ) -> zbus::fdo::Result<()> {
        if let Some(tx) = self.elicitation_txs.lock().await.remove(session_id) {
            let _ = tx.send(answer.to_string());
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn status_changed(
        ctxt: &SignalContext<'_>,
        session_id: &str,
        state: &str,
        context_pct: f64,
        model_name: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn elicitation_requested(
        ctxt: &SignalContext<'_>,
        session_id: &str,
        prompt: &str,
        options: &[&str],
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn session_removed(ctxt: &SignalContext<'_>, session_id: &str) -> zbus::Result<()>;
}

pub async fn set_state(
    sessions: &Sessions,
    ctxt: &SignalContext<'_>,
    session_id: &str,
    new_state: &str,
) -> zbus::fdo::Result<()> {
    let mut map = sessions.lock().await;
    let data = map
        .entry(session_id.to_string())
        .or_insert_with(ClaudeData::default);
    if new_state != "attention" {
        data.pre_attention_state = new_state.to_string();
    }
    data.state = new_state.to_string();
    let (ctx_pct, model_name) = (data.context_used_pct, data.model_name.clone());
    drop(map);
    ClaudeStatus::status_changed(ctxt, session_id, new_state, ctx_pct, &model_name).await?;
    Ok(())
}

pub async fn restore_after_attention(
    sessions: &Sessions,
    ctxt: &SignalContext<'_>,
    session_id: &str,
) -> zbus::fdo::Result<()> {
    let mut map = sessions.lock().await;
    let data = map
        .entry(session_id.to_string())
        .or_insert_with(ClaudeData::default);
    let restore = data.pre_attention_state.clone();
    data.state = restore.clone();
    let (ctx_pct, model_name) = (data.context_used_pct, data.model_name.clone());
    drop(map);
    ClaudeStatus::status_changed(ctxt, session_id, &restore, ctx_pct, &model_name).await?;
    Ok(())
}
