//! Human-in-the-loop approval broker.

use std::collections::HashMap;
use std::sync::Mutex;

use agentcore_core::{Action, Principal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: Uuid,
    pub action_id: Uuid,
    pub action: Action,
    pub reason: String,
    pub requested_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub approved: bool,
    pub by: Principal,
    pub comment: Option<String>,
}

struct Entry {
    info: PendingApproval,
    reply: oneshot::Sender<ApprovalDecision>,
}

#[derive(Default)]
pub(crate) struct ApprovalBroker {
    pending: Mutex<HashMap<Uuid, Entry>>,
}

impl ApprovalBroker {
    pub(crate) fn open(&self, info: PendingApproval) -> oneshot::Receiver<ApprovalDecision> {
        let (reply, rx) = oneshot::channel();
        self.lock().insert(info.approval_id, Entry { info, reply });
        rx
    }

    /// Deliver a decision. Returns false if the approval no longer exists.
    pub(crate) fn resolve(&self, approval_id: Uuid, decision: ApprovalDecision) -> bool {
        match self.lock().remove(&approval_id) {
            Some(entry) => entry.reply.send(decision).is_ok(),
            None => false,
        }
    }

    /// Forget an approval whose waiter gave up (timeout, stop).
    pub(crate) fn withdraw(&self, approval_id: Uuid) {
        self.lock().remove(&approval_id);
    }

    /// Drop every pending approval; waiters observe a closed channel.
    pub(crate) fn cancel_all(&self) {
        self.lock().clear();
    }

    pub(crate) fn list(&self) -> Vec<PendingApproval> {
        let mut list: Vec<_> = self.lock().values().map(|e| e.info.clone()).collect();
        list.sort_by_key(|p| p.requested_at);
        list
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Entry>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}
