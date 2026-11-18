//! Cost accounting: pricing, budget checks and the existing JSONL ledger.
//! Configuration lives in `zeroclaw-config`; shared records live in `zeroclaw-api`.

mod pricing;
pub mod tracker;

pub use pricing::calculate_token_usage;
pub use reiclaw_api::cost as types;
pub use reiclaw_api::cost::*;
pub use tracker::CostTracker;
