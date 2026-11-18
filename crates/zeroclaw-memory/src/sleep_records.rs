//! Typed sleep-memory records and invariants for ADR-025.
//!
//! Re-exported from [`zeroclaw_api::sleep_records`] so existing
//! `zeroclaw_memory::sleep_records::*` imports stay intact across crates.

#[allow(unused_imports)]
pub use zeroclaw_api::sleep_records::{
    ActionCandidateStatus, DailyDigest, EpisodeRecord, EpisodeSourceKind, PINNED_DIGEST_MAX_BYTES,
    PinnedDigest, validate_agent_id, validate_non_soul_key, validate_non_soul_namespace,
};
