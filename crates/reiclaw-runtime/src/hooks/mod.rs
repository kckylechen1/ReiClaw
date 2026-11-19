pub mod builtin;
mod runner;
mod traits;

pub use runner::HookRunner;
// HookHandler and HookResult are part of the crate's public hook API surface.
// They may appear unused internally but are intentionally re-exported for
// external integrations and future plugin authors.
#[allow(unused_imports)]
pub use traits::{HookHandler, HookResult};

tokio::task_local! {
    /// Per-call correlation identifier scoped around `before_tool_call` and
    /// `on_after_tool_call` so hooks correlate args per invocation rather than
    /// sharing a per-tool FIFO queue across concurrent/cancelled calls.
    pub static CURRENT_HOOK_CALL_ID: String;
}

pub fn current_hook_call_id() -> Option<String> {
    CURRENT_HOOK_CALL_ID.try_with(Clone::clone).ok()
}
