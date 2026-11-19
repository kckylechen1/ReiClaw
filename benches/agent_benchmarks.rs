//! Performance benchmarks for ZeroClaw hot paths.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::{Arc, Mutex};

use reiclaw::agent::agent::Agent;
use reiclaw::agent::dispatcher::NativeToolDispatcher;
use reiclaw::config::MemoryConfig;
use reiclaw::memory;
use reiclaw::memory::{Memory, MemoryCategory};
use reiclaw::observability::{NoopObserver, Observer};
use reiclaw::providers::{ChatRequest, ChatResponse, ModelProvider, ToolCall};
use reiclaw::tools::{Tool, ToolOutput, ToolResult};

use anyhow::Result;
use async_trait::async_trait;

// ─────────────────────────────────────────────────────────────────────────────
// Mock infrastructure (mirrors test mocks, kept local for benchmark isolation)
// ─────────────────────────────────────────────────────────────────────────────

struct BenchModelProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

impl BenchModelProvider {
    fn text_only(text: &str) -> Self {
        Self {
            responses: Mutex::new(vec![ChatResponse {
                text: Some(text.into()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            }]),
        }
    }

    fn with_tool_then_text() -> Self {
        Self {
            responses: Mutex::new(vec![
                ChatResponse {
                    text: Some(String::new()),
                    tool_calls: vec![ToolCall {
                        id: "tc1".into(),
                        name: "noop".into(),
                        arguments: "{}".into(),
                        extra_content: None,
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
            ]),
        }
    }
}

impl ::reiclaw_api::attribution::Attributable for BenchModelProvider {
    fn role(&self) -> ::reiclaw_api::attribution::Role {
        ::reiclaw_api::attribution::Role::Provider(::reiclaw_api::attribution::ProviderKind::Model(
            ::reiclaw_api::attribution::ModelProviderKind::Custom,
        ))
    }
    fn alias(&self) -> &str {
        "BenchModelProvider"
    }
}

#[async_trait]
impl ModelProvider for BenchModelProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> Result<String> {
        Ok("fallback".into())
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: Option<f64>,
    ) -> Result<ChatResponse> {
        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            });
        }
        Ok(guard.remove(0))
    }
}

struct NoopTool;

reiclaw_api::mock_tool_attribution!(NoopTool);

#[async_trait]
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "Does nothing"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: ToolOutput::default(),
            error: None,
        })
    }
}

fn make_memory() -> Arc<dyn Memory> {
    let cfg = MemoryConfig {
        backend: "none".into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, std::path::Path::new("/tmp"), None).unwrap())
}

fn make_bench_memory(dir: &std::path::Path) -> Arc<dyn Memory> {
    let backend = if memory::MEMORYCORE_COMPILED {
        "memorycore"
    } else {
        "markdown"
    };
    let cfg = MemoryConfig {
        backend: backend.into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, dir, None).unwrap())
}

fn make_observer() -> Arc<dyn Observer> {
    Arc::from(NoopObserver {})
}

// ─────────────────────────────────────────────────────────────────────────────
// Benchmark: Memory store + recall
// ─────────────────────────────────────────────────────────────────────────────

fn bench_memory_operations(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = make_bench_memory(tmp.path());

    // Seed with entries for recall benchmarks
    rt.block_on(async {
        for i in 0..100 {
            mem.store(
                &format!("key_{i}"),
                &format!("Content entry number {i} about zeroclaw agent runtime"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }
    });

    c.bench_function("memory_store_single", |b| {
        let counter = std::sync::atomic::AtomicUsize::new(1000);
        b.iter(|| {
            let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            rt.block_on(async {
                mem.store(
                    &format!("bench_key_{idx}"),
                    "Benchmark content for store operation",
                    MemoryCategory::Daily,
                    None,
                )
                .await
                .unwrap();
            });
        });
    });

    c.bench_function("memory_recall_top10", |b| {
        b.iter(|| {
            rt.block_on(async {
                mem.recall(black_box("zeroclaw agent"), 10, None, None, None)
                    .await
                    .unwrap()
            })
        });
    });

    c.bench_function("memory_count", |b| {
        b.iter(|| rt.block_on(async { mem.count().await.unwrap() }));
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Benchmark: Full agent turn cycle
// ─────────────────────────────────────────────────────────────────────────────

fn bench_agent_turn(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("agent_turn_text_only", |b| {
        b.iter(|| {
            rt.block_on(async {
                let model_provider = Box::new(BenchModelProvider::text_only("benchmark response"));
                let mut agent = Agent::builder()
                    .model_provider(model_provider)
                    .tools(vec![Box::new(NoopTool) as Box<dyn Tool>])
                    .memory(make_memory())
                    .observer(make_observer())
                    .tool_dispatcher(Box::new(NativeToolDispatcher))
                    .workspace_dir(std::path::PathBuf::from("/tmp"))
                    .build()
                    .unwrap();
                agent.turn(black_box("hello")).await.unwrap()
            })
        });
    });

    c.bench_function("agent_turn_with_tool_call", |b| {
        b.iter(|| {
            rt.block_on(async {
                let model_provider = Box::new(BenchModelProvider::with_tool_then_text());
                let mut agent = Agent::builder()
                    .model_provider(model_provider)
                    .tools(vec![Box::new(NoopTool) as Box<dyn Tool>])
                    .memory(make_memory())
                    .observer(make_observer())
                    .tool_dispatcher(Box::new(NativeToolDispatcher))
                    .workspace_dir(std::path::PathBuf::from("/tmp"))
                    .build()
                    .unwrap();
                agent.turn(black_box("run tool")).await.unwrap()
            })
        });
    });
}

criterion_group!(benches, bench_memory_operations, bench_agent_turn);
criterion_main!(benches);
