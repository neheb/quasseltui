//! `BacklogManager`: the receive side of backlog requests.
//!
//! The client sends `requestBacklog(BufferId, MsgId first, MsgId last, int
//! limit, int additional)`; the core answers with
//! `receiveBacklog(BufferId, MsgId, MsgId, int, int, QVariantList)` where
//! the list holds `Message` values. The slot stashes them and the
//! dispatcher merges them into state.

use crate::protocol::types::Message;
use crate::protocol::usertypes::UserValue;
use crate::qt::variant::Variant;
use crate::sync::{exact_params, log_unknown_slot};

pub const CLASS_NAME: &[u8] = b"BacklogManager";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BacklogManager {
    pub last_received: Vec<Message>,
    /// The buffer id from the slot parameters. It is authoritative over the
    /// ids inside the payload messages, so a confused or hostile core can't
    /// mix buffers.
    pub last_buffer_id: Option<Variant>,
}

impl BacklogManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_sync(&mut self, slot: &[u8], params: &[Variant]) {
        match slot {
            b"receiveBacklog" => {
                let Some([buffer_id, _first, _last, _limit, _additional, messages]) =
                    exact_params::<6>("BacklogManager", "", slot, params)
                else {
                    return;
                };
                // Record the buffer even when the payload is malformed, so
                // the requester still gets a (zero-count) completion.
                self.last_buffer_id = Some(buffer_id.clone());
                let Variant::List(items) = messages else {
                    tracing::warn!(
                        "receiveBacklog: expected list of Messages, got {}",
                        messages.type_name()
                    );
                    self.last_received.clear();
                    return;
                };
                self.last_received = items
                    .iter()
                    .filter_map(|item| match item {
                        Variant::User(UserValue::Message(m)) => Some((**m).clone()),
                        _ => None,
                    })
                    .collect();
                tracing::debug!(
                    "receiveBacklog for buffer {:?}: {} messages",
                    buffer_id,
                    self.last_received.len()
                );
            }
            _ => log_unknown_slot("BacklogManager", "", slot, params),
        }
    }
}
