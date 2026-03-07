use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

#[derive(Clone)]
pub struct ClaudeData {
    pub state: String,
    pub pre_attention_state: String,
    pub context_used_pct: f64,
    pub model_name: String,
}

impl Default for ClaudeData {
    fn default() -> Self {
        Self {
            state: "no-session".to_string(),
            pre_attention_state: "thinking".to_string(),
            context_used_pct: 0.0,
            model_name: String::new(),
        }
    }
}

pub type Sessions = Arc<Mutex<HashMap<String, ClaudeData>>>;
pub type ElicitationTxs = Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>;
