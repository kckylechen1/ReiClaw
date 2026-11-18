//! Compatibility exports while the retired TaskIntent port remains.
//! The Staff transport belongs to the tool crate; shared receipts to the API crate.

pub use zeroclaw_tools::tachi_delegation::{
    MAX_PROXY_CONTINUATIONS_PER_DISPATCH, MAX_TASK_CONTEXT_BRIEF_BYTES, ProxyContinuationError,
    ProxyContinuationGate, TASK_CONTEXT_HEADER, TaskBriefError, find_protected_identity_marker,
    format_redacted_task_with_brief, prepare_outbound_task_with_brief, validate_proxy_continuation,
};
pub use zeroclaw_tools::tachi_staff::*;
