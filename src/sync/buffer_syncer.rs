//! `BufferSyncer`: the singleton (object name `""`) carrying per-buffer
//! last-seen and marker-line state plus buffer lifecycle (remove, rename,
//! merge).
//!
//! Lifecycle slots only record pending changes; the dispatcher drains them
//! after every BufferSyncer slot, updates `ClientState`, and emits events.

use std::collections::{BTreeMap, BTreeSet};

use crate::qt::variant::{Variant, VariantMap};
use crate::sync::{exact_params, log_unknown_field, log_unknown_slot};

pub const CLASS_NAME: &[u8] = b"BufferSyncer";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BufferSyncer {
    pub initialized: bool,
    pub last_seen_by_buffer: BTreeMap<i64, i64>,
    pub marker_lines_by_buffer: BTreeMap<i64, i64>,
    /// Buffers removed (or merged away) since the last drain.
    pub removed_buffers: BTreeSet<i64>,
    /// Buffers renamed since the last drain, with their new names.
    pub renamed_buffers: BTreeMap<i64, String>,
}

/// A `{bufferId: msgId}` map arrives either as a QVariantMap (modern
/// cores) or as a flat `[id, msg, id, msg, ...]` list (older cores).
fn pairs(value: &Variant) -> Vec<(Variant, Variant)> {
    match value {
        Variant::Map(map) => map
            .iter()
            .map(|(k, v)| (Variant::String(k.clone()), v.clone()))
            .collect(),
        Variant::List(items) if items.len() % 2 == 0 => items
            .chunks_exact(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect(),
        _ => Vec::new(),
    }
}

impl BufferSyncer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_sync(&mut self, slot: &[u8], params: &[Variant]) {
        let one = || exact_params::<1>("BufferSyncer", "", slot, params);
        let two = || exact_params::<2>("BufferSyncer", "", slot, params);
        match slot {
            b"setLastSeenMsg" => {
                if let Some([bid, mid]) = two()
                    && let (Some(bid), Some(mid)) = (bid.coerce_i64(), mid.coerce_i64())
                {
                    self.last_seen_by_buffer.insert(bid, mid);
                }
            }
            b"setMarkerLine" => {
                if let Some([bid, mid]) = two()
                    && let (Some(bid), Some(mid)) = (bid.coerce_i64(), mid.coerce_i64())
                {
                    self.marker_lines_by_buffer.insert(bid, mid);
                }
            }
            b"removeBuffer" => {
                if let Some([bid]) = one()
                    && let Some(bid) = bid.coerce_i64()
                {
                    self.forget(bid);
                }
            }
            b"renameBuffer" => {
                if let Some([bid, name]) = two()
                    && let (Some(bid), Some(name)) = (bid.coerce_i64(), name.coerce_string())
                {
                    self.renamed_buffers.insert(bid, name);
                }
            }
            b"mergeBuffersPermanently" => {
                // The second buffer is merged into the first and goes away.
                if let Some([_, bid2]) = two()
                    && let Some(bid2) = bid2.coerce_i64()
                {
                    self.forget(bid2);
                }
            }
            // Core-side bookkeeping only; acknowledged so it isn't logged.
            b"markBufferAsRead" => {}
            _ => log_unknown_slot("BufferSyncer", "", slot, params),
        }
    }

    fn forget(&mut self, bid: i64) {
        self.removed_buffers.insert(bid);
        self.last_seen_by_buffer.remove(&bid);
        self.marker_lines_by_buffer.remove(&bid);
    }

    pub fn apply_init_data(&mut self, data: &VariantMap) {
        for (key, value) in data {
            self.apply_init_field(key, value);
        }
        self.initialized = true;
    }

    pub fn apply_init_field(&mut self, key: &str, value: &Variant) {
        let target = match key {
            "LastSeenMsg" => &mut self.last_seen_by_buffer,
            "MarkerLines" => &mut self.marker_lines_by_buffer,
            _ => {
                log_unknown_field("BufferSyncer", "", key, value);
                return;
            }
        };
        for (bid, mid) in pairs(value) {
            if let (Some(bid), Some(mid)) = (bid.coerce_i64(), mid.coerce_i64()) {
                target.insert(bid, mid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{BufferId, MsgId};
    use crate::protocol::usertypes::UserValue;

    fn bid(v: i32) -> Variant {
        Variant::User(UserValue::BufferId(BufferId(v)))
    }

    fn mid(v: i64) -> Variant {
        Variant::User(UserValue::MsgId(MsgId(v)))
    }

    #[test]
    fn last_seen_and_marker_slots() {
        let mut s = BufferSyncer::new();
        s.handle_sync(b"setLastSeenMsg", &[bid(1), mid(100)]);
        s.handle_sync(b"setMarkerLine", &[bid(1), mid(90)]);
        assert_eq!(s.last_seen_by_buffer[&1], 100);
        assert_eq!(s.marker_lines_by_buffer[&1], 90);
        s.handle_sync(b"setLastSeenMsg", &[Variant::Bool(true), mid(1)]);
        assert_eq!(s.last_seen_by_buffer.len(), 1);
    }

    #[test]
    fn remove_rename_merge() {
        let mut s = BufferSyncer::new();
        s.handle_sync(b"setLastSeenMsg", &[bid(5), mid(1)]);
        s.handle_sync(b"removeBuffer", &[bid(5)]);
        assert!(s.removed_buffers.contains(&5));
        assert!(!s.last_seen_by_buffer.contains_key(&5));
        s.handle_sync(b"renameBuffer", &[bid(6), "#new".into()]);
        assert_eq!(s.renamed_buffers[&6], "#new");
        s.handle_sync(b"mergeBuffersPermanently", &[bid(7), bid(8)]);
        assert!(s.removed_buffers.contains(&8));
        assert!(!s.removed_buffers.contains(&7));
    }

    #[test]
    fn init_accepts_map_and_flat_list() {
        let mut s = BufferSyncer::new();
        let mut map = VariantMap::new();
        map.insert("1".into(), mid(10));
        map.insert("2".into(), mid(20));
        s.apply_init_field("LastSeenMsg", &Variant::Map(map));
        assert_eq!(s.last_seen_by_buffer[&2], 20);
        s.apply_init_field(
            "MarkerLines",
            &Variant::List(vec![bid(3), mid(30), bid(4), mid(40)]),
        );
        assert_eq!(s.marker_lines_by_buffer[&4], 40);
        // Odd-length lists are ignored.
        s.apply_init_field("MarkerLines", &Variant::List(vec![bid(9)]));
        assert!(!s.marker_lines_by_buffer.contains_key(&9));
    }
}
