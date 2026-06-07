use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::oneshot;

#[derive(Clone, Default)]
pub(crate) struct RequestBroker {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl RequestBroker {
    pub(crate) fn register(&self, request_id: String, tx: oneshot::Sender<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(request_id, tx);
        }
    }

    pub(crate) fn respond(&self, request_id: &str, answer: String) -> bool {
        let tx = self
            .inner
            .lock()
            .ok()
            .and_then(|mut inner| inner.remove(request_id));
        tx.is_some_and(|tx| tx.send(answer).is_ok())
    }

    pub(crate) fn cancel(&self, request_id: &str) -> bool {
        self.respond(request_id, String::new())
    }

    pub(crate) fn remove(&self, request_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.remove(request_id);
        }
    }
}

pub(crate) fn global_request_broker() -> RequestBroker {
    static BROKER: OnceLock<RequestBroker> = OnceLock::new();
    BROKER.get_or_init(RequestBroker::default).clone()
}
