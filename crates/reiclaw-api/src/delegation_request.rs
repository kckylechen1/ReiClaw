//! Local ingress request references for Tachi-controlled delegation.
//!
//! These types describe only whether the body has claimed a request and which
//! canonical dispatch it references. Tachi remains the owner of run state.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Host-admitted provenance of the session that originated a delegated task.
/// This is never a model/tool argument. It is immutable once the request is claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationOrigin {
    /// Exact persisted transcript key, not an inferred agent-wide search scope.
    pub session_key: String,
    pub scope: DelegationOriginScope,
}

/// Provenance alone does not authorize reading the owner's facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationOriginScope {
    /// An admitted source (for example a device) with no automatic fact access.
    SourceOnly,
    /// Version-one grant for this session only, subject to live revalidation.
    SessionFacts {
        /// Hash of the paired operator credential, never the bearer token.
        operator_subject: String,
        /// Stable identity pinned by the originating conversation.
        agent_id: String,
        /// Canonical memory ID. History transport keys are not memory IDs.
        memory_session_id: Option<String>,
    },
}

/// Resolve provenance from the authenticated host turn, not from generated text.
/// A present resolver returning `None` means the admitted source is no longer valid.
pub type DelegationOriginResolver = Arc<dyn Fn(&str) -> Option<DelegationOrigin> + Send + Sync>;

tokio::task_local! {
    pub static DELEGATION_ORIGIN_CONTEXT: DelegationOriginResolver;
}

/// The outcome of atomically claiming an agent-scoped delegation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationRequestClaim {
    /// The durable claim was created. Only this outcome permits submission.
    Created,
    /// This exact request was already claimed. `None` means the external
    /// outcome is unresolved; it must be reconciled rather than resubmitted.
    Existing { dispatch_id: Option<String> },
    /// The request ID was already used for a different payload, route or source.
    Conflict,
}

/// A durable local request's reference to canonical Tachi dispatch truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationRequestBinding {
    /// Digest of the admitted task/profile/arguments, separate from route provenance.
    pub request_digest: String,
    /// Immutable route fingerprint created with the claim. Legacy scoped rows
    /// derive it from their persisted key; an unscoped legacy row has no proof.
    pub route_digest: Option<String>,
    /// A known canonical dispatch ID, or an unresolved external outcome.
    pub dispatch_id: Option<String>,
    /// Original source only; legacy claims are never upgraded by a replay.
    pub origin: Option<DelegationOrigin>,
}
