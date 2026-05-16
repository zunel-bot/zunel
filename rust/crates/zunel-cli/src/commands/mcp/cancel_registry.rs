//! In-memory map of in-flight JSON-RPC request ids to cancellation
//! tokens. Slice 2 wires this to:
//!
//! * `helper_ask`: registers a fresh token under the inbound request
//!   id when a call starts and removes the entry when the call
//!   returns (the [`CancelGuard`] drop handles that automatically so
//!   panics can't leak entries).
//! * The dispatcher: routes `notifications/cancelled` from the hub
//!   to [`CancelRegistry::cancel`], which fires the matching token.
//!
//! The registry is intentionally narrow — no per-request metadata, no
//! state-machine. Anything that needs more (e.g. an audit log of
//! cancelled calls) layers on top.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use zunel_tools::RpcId;

/// Shared, threadsafe registry of in-flight calls keyed by their
/// JSON-RPC `id`.
#[derive(Default, Debug)]
pub struct CancelRegistry {
    inner: Mutex<HashMap<RpcId, CancellationToken>>,
}

impl CancelRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a fresh [`CancellationToken`] under `id` and return a
    /// [`CancelGuard`] that owns the token's lifecycle: when the
    /// guard drops the entry is removed, so a panic during call
    /// processing can't leak the slot.
    ///
    /// If the id was already registered (shouldn't happen for
    /// well-behaved JSON-RPC clients, but spec doesn't forbid id
    /// reuse across batches) we install our fresh token and the
    /// previous one stays orphaned — its owner sees their CancelGuard
    /// drop normally and life goes on. The narrower contract avoids
    /// a "first-writer-wins or last-writer-wins" coordination
    /// problem we don't need to solve.
    pub fn register(self: &Arc<Self>, id: RpcId) -> CancelGuard {
        let token = CancellationToken::new();
        let mut map = self.inner.lock().expect("cancel registry mutex");
        map.insert(id.clone(), token.clone());
        CancelGuard {
            registry: Arc::clone(self),
            id,
            token,
        }
    }

    /// Fire the cancel token for `id` if one is registered. Returns
    /// `true` when the lookup found a target — useful for the
    /// dispatcher to decide whether to ack the cancellation
    /// notification or warn-log the unknown-id case.
    pub fn cancel(&self, id: &RpcId) -> bool {
        let map = self.inner.lock().expect("cancel registry mutex");
        if let Some(token) = map.get(id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Snapshot the number of in-flight registrations. Test-only
    /// helper.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().expect("cancel registry mutex").len()
    }

    fn unregister(&self, id: &RpcId) {
        let mut map = self.inner.lock().expect("cancel registry mutex");
        map.remove(id);
    }
}

/// RAII handle returned by [`CancelRegistry::register`]. Drops the
/// entry from the registry when it goes out of scope.
pub struct CancelGuard {
    registry: Arc<CancelRegistry>,
    id: RpcId,
    token: CancellationToken,
}

impl CancelGuard {
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.registry.unregister(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_returns_a_fresh_token_per_id() {
        let reg = CancelRegistry::new();
        let g1 = reg.register(RpcId::Number(1));
        let g2 = reg.register(RpcId::Number(2));
        assert!(!g1.token().is_cancelled());
        assert!(!g2.token().is_cancelled());
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn cancel_fires_the_matching_token() {
        let reg = CancelRegistry::new();
        let g = reg.register(RpcId::String("req-7".into()));
        let token = g.token();
        assert!(reg.cancel(&RpcId::String("req-7".into())));
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_returns_false_for_unknown_id() {
        let reg = CancelRegistry::new();
        assert!(!reg.cancel(&RpcId::Number(404)));
    }

    #[test]
    fn drop_removes_the_entry() {
        let reg = CancelRegistry::new();
        {
            let _g = reg.register(RpcId::Number(1));
            assert_eq!(reg.len(), 1);
        }
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn cancelling_after_drop_is_a_noop() {
        let reg = CancelRegistry::new();
        let token;
        {
            let g = reg.register(RpcId::Number(99));
            token = g.token();
        }
        // Guard dropped, so cancel by id is now a noop and the
        // already-handed-out token stays uncancelled.
        assert!(!reg.cancel(&RpcId::Number(99)));
        assert!(!token.is_cancelled());
    }
}
