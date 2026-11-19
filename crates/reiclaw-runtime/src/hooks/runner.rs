use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, future::join_all};
use serde_json::Value;
use std::panic::AssertUnwindSafe;

use reiclaw_api::model_provider::ChatMessage;
use reiclaw_api::tool::ToolResult;

use super::traits::{HookHandler, HookResult};
use crate::security::estop::EstopHandle;

pub struct HookRunner {
    handlers: Vec<Box<dyn HookHandler>>,
}

impl Default for HookRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRunner {
    /// Create an empty runner with no handlers.
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn from_config(hooks: &reiclaw_config::schema::HooksConfig) -> Self {
        let mut runner = Self::new();
        if hooks.builtin.command_logger {
            runner.register(Box::new(super::builtin::CommandLoggerHook::new()));
        }
        if hooks.builtin.webhook_audit.enabled {
            runner.register(Box::new(super::builtin::WebhookAuditHook::new(
                hooks.builtin.webhook_audit.clone(),
            )));
        }
        runner
    }

    /// Build a runner from config without panicking on invalid builtin hook settings.
    pub fn try_from_config(hooks: &reiclaw_config::schema::HooksConfig) -> anyhow::Result<Self> {
        Self::try_from_config_with_estop(hooks, None)
    }

    /// Build a runner from config with an optional live estop handle attached to outbound hooks.
    pub fn try_from_config_with_estop(
        hooks: &reiclaw_config::schema::HooksConfig,
        estop: Option<Arc<EstopHandle>>,
    ) -> anyhow::Result<Self> {
        let mut runner = Self::new();
        if hooks.builtin.command_logger {
            runner.register(Box::new(super::builtin::CommandLoggerHook::new()));
        }
        if hooks.builtin.webhook_audit.enabled {
            let audit =
                super::builtin::WebhookAuditHook::try_new(hooks.builtin.webhook_audit.clone())?
                    .with_estop(estop);
            runner.register(Box::new(audit));
        }
        Ok(runner)
    }

    /// Register a handler and re-sort by descending priority.
    pub fn register(&mut self, handler: Box<dyn HookHandler>) {
        self.handlers.push(handler);
        self.handlers
            .sort_by_key(|h| std::cmp::Reverse(h.priority()));
    }

    // ---------------------------------------------------------------
    // Void dispatchers (parallel, fire-and-forget)
    // ---------------------------------------------------------------

    pub async fn fire_gateway_start(&self, host: &str, port: u16) {
        let futs: Vec<_> = self
            .handlers
            .iter()
            .map(|h| h.on_gateway_start(host, port))
            .collect();
        join_all(futs).await;
    }

    pub async fn fire_llm_input(&self, messages: &[ChatMessage], model: &str) {
        let futs: Vec<_> = self
            .handlers
            .iter()
            .map(|h| h.on_llm_input(messages, model))
            .collect();
        join_all(futs).await;
    }

    pub async fn fire_after_tool_call(&self, tool: &str, result: &ToolResult, duration: Duration) {
        let futs: Vec<_> = self
            .handlers
            .iter()
            .map(|h| h.on_after_tool_call(tool, result, duration))
            .collect();
        join_all(futs).await;
    }

    // ---------------------------------------------------------------
    // Modifying dispatchers (sequential by priority, short-circuit on Cancel)
    // ---------------------------------------------------------------

    pub async fn run_before_tool_call(
        &self,
        mut name: String,
        mut args: Value,
    ) -> HookResult<(String, Value)> {
        for h in &self.handlers {
            let hook_name = h.name();
            match AssertUnwindSafe(h.before_tool_call(name.clone(), args.clone()))
                .catch_unwind()
                .await
            {
                Ok(HookResult::Continue((n, a))) => {
                    name = n;
                    args = a;
                }
                Ok(HookResult::Cancel(reason)) => {
                    // The turn owns logging the cancellation body under the
                    // tool's live I/O policy. Keep only hook identity here.
                    ::reiclaw_log::record!(
                        INFO,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"hook": hook_name})),
                        "before_tool_call cancelled by hook"
                    );
                    return HookResult::Cancel(reason);
                }
                Err(_) => {
                    ::reiclaw_log::record!(
                        ERROR,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"hook": hook_name})),
                        "before_tool_call hook panicked; continuing with previous values"
                    );
                }
            }
        }
        HookResult::Continue((name, args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A hook that records how many times void events fire.
    struct CountingHook {
        name: String,
        priority: i32,
        fire_count: Arc<AtomicU32>,
    }

    impl CountingHook {
        fn new(name: &str, priority: i32) -> (Self, Arc<AtomicU32>) {
            let count = Arc::new(AtomicU32::new(0));
            (
                Self {
                    name: name.to_string(),
                    priority,
                    fire_count: count.clone(),
                },
                count,
            )
        }
    }

    #[async_trait]
    impl HookHandler for CountingHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }
        async fn on_after_tool_call(&self, _tool: &str, _result: &ToolResult, _duration: Duration) {
            self.fire_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A modifying hook that uppercases the tool name.
    struct UppercaseToolHook {
        name: String,
        priority: i32,
    }

    #[async_trait]
    impl HookHandler for UppercaseToolHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }
        async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
            HookResult::Continue((name.to_uppercase(), args))
        }
    }

    /// A modifying hook that cancels `before_tool_call`.
    struct CancelToolHook {
        name: String,
        priority: i32,
    }

    #[async_trait]
    impl HookHandler for CancelToolHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }
        async fn before_tool_call(
            &self,
            _name: String,
            _args: Value,
        ) -> HookResult<(String, Value)> {
            HookResult::Cancel("blocked by policy".into())
        }
    }

    /// A modifying hook that appends a suffix to the tool name.
    struct SuffixToolHook {
        name: String,
        priority: i32,
        suffix: String,
    }

    #[async_trait]
    impl HookHandler for SuffixToolHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }
        async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
            HookResult::Continue((format!("{}{}", name, self.suffix), args))
        }
    }

    #[test]
    fn register_and_sort_by_priority() {
        let mut runner = HookRunner::new();
        let (low, _) = CountingHook::new("low", 1);
        let (high, _) = CountingHook::new("high", 10);
        let (mid, _) = CountingHook::new("mid", 5);

        runner.register(Box::new(low));
        runner.register(Box::new(high));
        runner.register(Box::new(mid));

        let names: Vec<&str> = runner.handlers.iter().map(|h| h.name()).collect();
        assert_eq!(names, vec!["high", "mid", "low"]);
    }

    #[tokio::test]
    async fn void_hooks_fire_all_handlers() {
        let mut runner = HookRunner::new();
        let (h1, c1) = CountingHook::new("hook_a", 0);
        let (h2, c2) = CountingHook::new("hook_b", 0);

        runner.register(Box::new(h1));
        runner.register(Box::new(h2));

        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        runner
            .fire_after_tool_call("echo", &result, Duration::from_millis(1))
            .await;

        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn modifying_hook_can_cancel() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(CancelToolHook {
            name: "blocker".into(),
            priority: 10,
        }));
        runner.register(Box::new(UppercaseToolHook {
            name: "upper".into(),
            priority: 0,
        }));

        let result = runner
            .run_before_tool_call("hello".into(), Value::Null)
            .await;
        assert!(result.is_cancel());
    }

    #[tokio::test]
    async fn modifying_hook_pipelines_data() {
        let mut runner = HookRunner::new();

        // Priority 10 runs first: uppercases
        runner.register(Box::new(UppercaseToolHook {
            name: "upper".into(),
            priority: 10,
        }));
        // Priority 0 runs second: appends suffix
        runner.register(Box::new(SuffixToolHook {
            name: "suffix".into(),
            priority: 0,
            suffix: "_done".into(),
        }));

        match runner
            .run_before_tool_call("hello".into(), Value::Null)
            .await
        {
            HookResult::Continue((result, _)) => assert_eq!(result, "HELLO_done"),
            HookResult::Cancel(_) => panic!("should not cancel"),
        }
    }

    /// A hook that panics on `before_tool_call`. Exercises the `catch_unwind`
    /// branch in the runner.
    struct PanickingHook {
        name: String,
        priority: i32,
    }

    #[async_trait]
    impl HookHandler for PanickingHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }

        async fn before_tool_call(
            &self,
            _name: String,
            _args: Value,
        ) -> HookResult<(String, Value)> {
            panic!("simulated before_tool_call panic");
        }
    }

    #[tokio::test]
    async fn panicking_before_tool_call_does_not_break_subsequent_handler() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(PanickingHook {
            name: "panicker".into(),
            priority: 10,
        }));

        // A modifying hook that renames the tool call so we can verify it
        // ran after the panicker.
        struct RenameToolHook {
            name: String,
            priority: i32,
        }
        #[async_trait]
        impl HookHandler for RenameToolHook {
            fn name(&self) -> &str {
                &self.name
            }
            fn priority(&self) -> i32 {
                self.priority
            }
            async fn before_tool_call(
                &self,
                name: String,
                _args: Value,
            ) -> HookResult<(String, Value)> {
                HookResult::Continue((format!("{name}_renamed"), Value::Null))
            }
        }

        runner.register(Box::new(RenameToolHook {
            name: "renamer".into(),
            priority: 0,
        }));

        let result = runner
            .run_before_tool_call("shell".into(), Value::Null)
            .await;
        match result {
            HookResult::Continue((name, _)) => {
                assert_eq!(
                    name, "shell_renamed",
                    "hook after panicker must run and apply its modification"
                );
            }
            HookResult::Cancel(_) => panic!("panicking hook must not cancel"),
        }
    }

    #[tokio::test]
    async fn cancelling_before_tool_call_short_circuits_remaining_handlers() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(CancelToolHook {
            name: "blocker".into(),
            priority: 10,
        }));

        struct ToolCallCounterHook {
            name: String,
            priority: i32,
            count: Arc<AtomicU32>,
        }
        #[async_trait]
        impl HookHandler for ToolCallCounterHook {
            fn name(&self) -> &str {
                &self.name
            }
            fn priority(&self) -> i32 {
                self.priority
            }
            async fn before_tool_call(
                &self,
                name: String,
                args: Value,
            ) -> HookResult<(String, Value)> {
                self.count.fetch_add(1, Ordering::SeqCst);
                HookResult::Continue((name, args))
            }
        }

        let count = Arc::new(AtomicU32::new(0));
        runner.register(Box::new(ToolCallCounterHook {
            name: "counter".into(),
            priority: 0,
            count: Arc::clone(&count),
        }));

        let result = runner
            .run_before_tool_call("shell".into(), Value::Null)
            .await;
        assert!(
            result.is_cancel(),
            "canceller must short-circuit the run with HookResult::Cancel"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "hooks after the canceller must NOT run"
        );
    }

    // ── from_config / try_from_config tests ──────────────────────

    #[test]
    fn from_config_disabled_builtins_produces_empty_runner() {
        let config = reiclaw_config::schema::HooksConfig {
            enabled: true,
            builtin: reiclaw_config::schema::BuiltinHooksConfig {
                command_logger: false,
                webhook_audit: reiclaw_config::schema::WebhookAuditConfig::default(),
            },
        };
        let runner = HookRunner::from_config(&config);
        assert!(
            runner.handlers.is_empty(),
            "no builtins enabled → runner must be empty"
        );
    }

    #[test]
    fn from_config_registers_command_logger_when_enabled() {
        let config = reiclaw_config::schema::HooksConfig {
            enabled: true,
            builtin: reiclaw_config::schema::BuiltinHooksConfig {
                command_logger: true,
                webhook_audit: reiclaw_config::schema::WebhookAuditConfig::default(),
            },
        };
        let runner = HookRunner::from_config(&config);
        let names: Vec<&str> = runner.handlers.iter().map(|h| h.name()).collect();
        assert!(
            names.contains(&"command-logger"),
            "command-logger enabled → must be registered; got {names:?}"
        );
    }

    #[test]
    fn try_from_config_rejects_invalid_webhook_audit_url() {
        let config = reiclaw_config::schema::HooksConfig {
            enabled: true,
            builtin: reiclaw_config::schema::BuiltinHooksConfig {
                command_logger: false,
                webhook_audit: reiclaw_config::schema::WebhookAuditConfig {
                    enabled: true,
                    url: "http://example.com/audit".to_string(),
                    tool_patterns: vec!["*".to_string()],
                    include_args: false,
                    max_args_bytes: 1024,
                },
            },
        };
        assert!(HookRunner::try_from_config(&config).is_err());
    }

    #[tokio::test]
    async fn empty_runner_lifecycle_events_are_noops() {
        let runner = HookRunner::new();
        // Must not panic when no handlers are registered.
        runner.fire_gateway_start("127.0.0.1", 3000).await;
        runner.fire_llm_input(&[], "stub").await;
    }
}
