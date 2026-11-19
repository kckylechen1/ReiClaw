//! V1 bounded ReasoningSubAgent vertical (collapsed ADR-026 S5).
//!
//! One bounded local reasoning run, end to end:
//!
//! ```text
//! Parent
//!   → SubAgentProfileV1     immutable, digest-verified, zero tools
//!   → ContextBundleV1       digest-bound content snapshot
//!   → admit()               the FIRST privacy boundary: Private Dyad /
//!                           AgentSoul refs rejected; AdmittedContextBundleV1
//!                           cannot express them
//!   → lineage_ref           the ONE depth authority
//!   → SubAgentRunV1         run-scoped principal
//!   → SubAgentReportV1      the ONLY child→parent result
//! ```
//!
//! Authority boundaries encoded here:
//! - V1 reasoning runs execute no tools (`profile.tool_policy.tools` must be
//!   empty at run admission).
//! - `SubAgentExecutionContextV1::new` accepts only the four bounded inputs
//!   (`objective`, `bundle`, `lineage`, `budget_meter`). No `Config`, tool
//!   registry, channel map, or memory backend crosses the parent→child
//!   boundary.
//! - Model access is a host-resolved opaque binding: the child never holds
//!   provider configuration or credential material.
//! - Nothing durable: this path writes no Task/Attempt rows anywhere and does
//!   not touch the control plane.
//! - Children of V1 cannot spawn children (D1): run admission refuses any
//!   spawning lineage deeper than the parent (depth 0).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use reiclaw_api::subagent_v1::{
    AdmittedContextBundleV1, BundleRedactionPolicy, ContextBundleV1, ContextClassV1, Finding,
    LineageRef, ModelPolicyV1, ParentRunRef, ProposedCandidate, Recommendation, SubAgentBudgetV1,
    SubAgentProfileV1, SubAgentReportV1, SubAgentRoleV1, SubAgentRunRef, SubAgentTerminalFact,
    SubAgentToolPolicyV1, SubAgentUsage, VersionedProfileRef,
};
use reiclaw_api::tool::{Tool, ToolOutput, ToolResult};
use reiclaw_config::schema::Config;

// ─────────────────────────────────────────────────────────────────────────
// Advisor per-turn budget (#405)
// ─────────────────────────────────────────────────────────────────────────

// Advisor consultations made in the current agent turn. The turn engine
// (`run_tool_call_loop`) owns the counter and installs a fresh one per
// turn via [`scope_advisor_turn`]; the advisor-bound `reasoning_subagent`
// only borrows it while it executes.
//
// Spawn hazard: a tokio task-local is NOT inherited by a spawned task. A
// tool future moved onto `spawn!`/`JoinSet::spawn` runs outside this scope,
// so it cannot see (or be capped by) the turn's counter. That is why a call
// outside the scope is refused rather than admitted: a lost scope must not
// silently lift the per-turn cap. Code that spawns tool execution must
// re-enter the scope in the spawned task with the parent's counter.
tokio::task_local! {
    static ADVISOR_TURN_CALLS: Arc<std::sync::atomic::AtomicU32>;
}

/// Run `future` as one agent turn for advisor accounting: advisor calls
/// made inside it count against a per-turn budget. A scope entered while
/// another is already active reuses its counter — a nested scope must not
/// mint a fresh budget that silently resets the outer turn's cap.
pub(crate) async fn scope_advisor_turn<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let counter = ADVISOR_TURN_CALLS
        .try_with(Arc::clone)
        .unwrap_or_else(|_| Arc::new(std::sync::atomic::AtomicU32::new(0)));
    ADVISOR_TURN_CALLS.scope(counter, future).await
}

/// The outcome of [`try_reserve_advisor_call`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdvisorReservation {
    Reserved,
    /// The turn already used `max_calls`.
    BudgetUsed,
    /// No turn scope is installed, so the per-turn cap cannot be enforced.
    NoTurnScope,
}

/// Reserve one advisor call in the current turn. Fails closed: outside a
/// turn scope (see the spawn hazard above) the call is refused.
fn try_reserve_advisor_call(max_calls: u32) -> AdvisorReservation {
    use std::sync::atomic::Ordering;
    match ADVISOR_TURN_CALLS.try_with(|calls| {
        calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < max_calls).then_some(used + 1)
            })
            .is_ok()
    }) {
        Ok(true) => AdvisorReservation::Reserved,
        Ok(false) => AdvisorReservation::BudgetUsed,
        Err(_) => AdvisorReservation::NoTurnScope,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Opaque model access binding (SA-7d)
// ─────────────────────────────────────────────────────────────────────────

/// A bounded completion request. Carries only prompt content and sampling
/// hints — no credentials, no provider configuration.
#[derive(Debug, Clone)]
pub struct BoundedModelRequest {
    pub system: String,
    pub user: String,
    pub temperature: Option<f64>,
}

/// A bounded completion response with the usage the meter needs.
#[derive(Debug, Clone)]
pub struct BoundedModelResponse {
    pub text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// The opaque model-access binding handed to the run driver. Its public
/// surface is redacted `Debug` and echoing the non-secret `provider_ref`;
/// no credential material or provider configuration is reachable from it.
#[derive(Clone)]
pub struct OpaqueModelBinding {
    inner: ModelBindingInner,
}

#[derive(Clone)]
enum ModelBindingInner {
    Config {
        config: Arc<Config>,
        policy: ModelPolicyV1,
    },
    #[cfg(test)]
    Stub(Arc<tests::StubResolver>),
}

impl std::fmt::Debug for OpaqueModelBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted by construction: only the non-secret reference prints.
        f.debug_struct("OpaqueModelBinding")
            .field("provider_ref", &self.provider_ref())
            .finish_non_exhaustive()
    }
}

impl OpaqueModelBinding {
    /// Construct a host-resolved opaque binding over the operator's provider
    /// configuration and the resolved `model_policy`.
    #[must_use]
    pub fn from_config(config: Arc<Config>, policy: ModelPolicyV1) -> Self {
        Self {
            inner: ModelBindingInner::Config { config, policy },
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_stub(stub: Arc<tests::StubResolver>) -> Self {
        Self {
            inner: ModelBindingInner::Stub(stub),
        }
    }

    /// Non-secret identity of the resolved reference (`family.alias`).
    #[must_use]
    pub fn provider_ref(&self) -> &str {
        match &self.inner {
            ModelBindingInner::Config { policy, .. } => &policy.provider_ref,
            #[cfg(test)]
            ModelBindingInner::Stub(_) => "stub.test",
        }
    }

    async fn complete(&self, request: BoundedModelRequest) -> anyhow::Result<BoundedModelResponse> {
        match &self.inner {
            ModelBindingInner::Config { config, policy } => {
                Self::complete_via_config(config, policy, request).await
            }
            #[cfg(test)]
            ModelBindingInner::Stub(stub) => stub.complete(request).await,
        }
    }

    async fn complete_via_config(
        config: &Config,
        policy: &ModelPolicyV1,
        request: BoundedModelRequest,
    ) -> anyhow::Result<BoundedModelResponse> {
        use reiclaw_api::model_provider::{ChatMessage, ChatRequest};

        let (family, alias) = policy.provider_ref.split_once('.').ok_or_else(|| {
            anyhow::Error::msg(format!(
                "model_policy.provider_ref {:?} is not family.alias",
                policy.provider_ref
            ))
        })?;
        let entry = config
            .providers
            .models
            .iter_entries()
            .find(|(ty, al, _)| *ty == family && *al == alias)
            .map(|(_, _, entry)| entry)
            .ok_or_else(|| {
                anyhow::Error::msg(format!(
                    "model_policy.provider_ref {:?} names no configured provider",
                    policy.provider_ref
                ))
            })?;
        let credential = entry.api_key.clone();
        let options = reiclaw_providers::provider_runtime_options_for_alias(config, family, alias);
        let provider = reiclaw_providers::create_model_provider_for_alias(
            config,
            family,
            alias,
            credential.as_deref(),
            &options,
        )?;
        // Resolve the configured model for the referenced alias when
        // the profile does not pin one — and FAIL CLOSED when neither
        // supplies a non-empty model: dispatching an empty model
        // identifier is an invalid request, never a guess.
        let model = match policy.model.clone() {
            Some(model) if !model.trim().is_empty() => model,
            _ => config
                .providers
                .models
                .iter_entries()
                .find(|(ty, al, _)| *ty == family && *al == alias)
                .and_then(|(_, _, entry)| entry.model.clone())
                .unwrap_or_default(),
        };
        if model.trim().is_empty() {
            return Err(anyhow::Error::msg(format!(
                "no model configured for provider_ref {:?}: neither the profile's \
                 model_policy nor the provider alias pins a model; refusing to \
                 dispatch an empty model identifier",
                policy.provider_ref
            )));
        }
        let messages = vec![
            ChatMessage::system(request.system),
            ChatMessage::user(request.user),
        ];
        let chat_request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        // Provider-dispatch contract: model calls go through
        // `ProviderDispatch` so attribution spans attach (never direct
        // `ModelProvider` method calls).
        let dispatcher = reiclaw_providers::ProviderDispatch::from_ref(provider.as_ref());
        let response = dispatcher
            .chat(chat_request, &model, request.temperature)
            .await?;
        // Attribute the child's spend to the resolved `type.alias` in the
        // turn's cost scope (gateway WS, `loop_::run`, `turn_entry`). No
        // scope (direct/test calls) = nothing is recorded.
        if let Some(usage) = response.usage.as_ref() {
            let _ = crate::agent::cost::record_tool_loop_cost_usage(
                &policy.provider_ref,
                &model,
                usage,
            );
        }
        let (tokens_in, tokens_out) = response
            .usage
            .map(|usage| {
                (
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        if tokens_in == 0 && tokens_out == 0 {
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                "subagent_v1: model binding resolved no token usage; token budget is enforced over counted usage only"
            );
        }
        Ok(BoundedModelResponse {
            text: response.text.unwrap_or_default(),
            tokens_in,
            tokens_out,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Budget meter (SA-8/SA-27)
// ─────────────────────────────────────────────────────────────────────────

/// Shared read-accounting meter (SA-8), scoped to ONE run: the SAME
/// `Arc` is held by the parent-side spawn admission and the child run,
/// so child consumption counts against that run's budget and
/// exhaustion terminates the run. The meter is minted fresh per
/// `SubAgentRunRef` and discarded at the run's terminal — it is
/// single-run state, NEVER process-lifetime quota.
pub struct SubAgentBudgetMeter {
    budget: SubAgentBudgetV1,
    state: Mutex<MeterState>,
}

#[derive(Clone, Copy)]
struct MeterState {
    actions_used: u32,
    tokens_in: u64,
    tokens_out: u64,
    started_at: std::time::Instant,
}

impl SubAgentBudgetMeter {
    #[must_use]
    pub fn new(budget: SubAgentBudgetV1) -> Self {
        Self::new_with_start(budget, std::time::Instant::now())
    }

    /// Constructor with an injected start instant: hosts and tests
    /// advance the meter's clock deterministically instead of sleeping.
    #[must_use]
    pub fn new_with_start(budget: SubAgentBudgetV1, started_at: std::time::Instant) -> Self {
        Self {
            budget,
            state: Mutex::new(MeterState {
                actions_used: 0,
                tokens_in: 0,
                tokens_out: 0,
                started_at,
            }),
        }
    }

    /// The budget this meter enforces (all three ceilings — SA-27).
    #[must_use]
    pub fn budget(&self) -> SubAgentBudgetV1 {
        self.budget
    }

    /// Record one billable action. `false` = the action ceiling is
    /// exhausted; the caller must not run the action.
    pub fn try_record_action(&self) -> bool {
        let mut state = self.state.lock();
        if state.actions_used >= self.budget.max_actions {
            return false;
        }
        state.actions_used += 1;
        true
    }

    /// Record token usage. `false` = the token ceiling is (now) exceeded;
    /// budget exhaustion terminates the run `timed_out` (SA-23).
    pub fn record_tokens(&self, tokens_in: u64, tokens_out: u64) -> bool {
        let mut state = self.state.lock();
        state.tokens_in = state.tokens_in.saturating_add(tokens_in);
        state.tokens_out = state.tokens_out.saturating_add(tokens_out);
        state.tokens_in.saturating_add(state.tokens_out) <= self.budget.max_tokens
    }

    /// Wall-clock exhaustion (SA-23: `timed_out` is budget exhaustion).
    #[must_use]
    pub fn time_exhausted(&self) -> bool {
        let state = self.state.lock();
        state.started_at.elapsed().as_secs() >= self.budget.time_limit_secs
    }

    #[must_use]
    pub fn exhausted(&self) -> bool {
        let state = self.state.lock();
        state.actions_used >= self.budget.max_actions
            || state.tokens_in.saturating_add(state.tokens_out) >= self.budget.max_tokens
            || state.started_at.elapsed().as_secs() >= self.budget.time_limit_secs
    }

    #[must_use]
    pub fn usage(&self) -> SubAgentUsage {
        let state = self.state.lock();
        SubAgentUsage {
            elapsed_ms: state.started_at.elapsed().as_millis() as u64,
            tokens_in: state.tokens_in,
            tokens_out: state.tokens_out,
            actions: state.actions_used,
        }
    }

    /// Remaining wall-clock, for the run's unit timeout.
    fn remaining_time(&self) -> std::time::Duration {
        let state = self.state.lock();
        let elapsed = state.started_at.elapsed();
        let ceiling = std::time::Duration::from_secs(self.budget.time_limit_secs);
        ceiling.checked_sub(elapsed).unwrap_or_default()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Default reasoning profile & run admission
// ─────────────────────────────────────────────────────────────────────────

/// The built-in default minimal Reasoning profile: zero tools, bounded
/// budget, structured report only.
pub const DEFAULT_REASONING_PROFILE_ID: &str = "default-reasoning-v1";

/// Construct the canonical V1 reasoning profile with its pinned digest.
#[must_use]
pub fn default_reasoning_profile() -> SubAgentProfileV1 {
    let mut profile = SubAgentProfileV1 {
        profile_id: DEFAULT_REASONING_PROFILE_ID.to_string(),
        revision: 1,
        digest: String::new(),
        role: SubAgentRoleV1::Reasoning,
        model_policy: ModelPolicyV1 {
            provider_ref: String::new(),
            model: None,
            temperature: None,
        },
        tool_policy: SubAgentToolPolicyV1::default(),
        supervisor_authority_set: Vec::new(),
        context_policy: reiclaw_api::subagent_v1::SubAgentContextPolicyV1 {
            allowed_classes: vec![
                ContextClassV1::ObjectiveContext,
                ContextClassV1::SourceRefs,
                ContextClassV1::UserModelProjection,
            ],
            max_projection_bytes: 65_536,
        },
        privacy_policy: reiclaw_api::subagent_v1::SubAgentPrivacyPolicyV1 {
            // Consistent with allowed_classes: public partitions only.
            // PrivateDyad is REDACTED unconditionally by `projection`
            // (SA-14.3 existence-blindness) and AgentSoul is REFUSED
            // unconditionally by `projection_with_policy` (SA-15) —
            // regardless of this list.
            permitted_partitions: vec![
                reiclaw_api::companion::SourcePartition::UserModel,
                reiclaw_api::companion::SourcePartition::SharedLexicon,
            ],
        },
        budget: SubAgentBudgetV1::default(),
        recursion: reiclaw_api::subagent_v1::SubAgentRecursionPolicyV1::NoLocalSpawn,
        output_schema: reiclaw_api::subagent_v1::SubAgentOutputSchemaV1::StructuredReport,
    };
    profile.digest = profile.compute_digest();
    profile
}

#[derive(Debug, thiserror::Error)]
pub enum RunAdmissionError {
    #[error("profile digest does not verify: {0}")]
    ProfileDigest(String),
    #[error(
        "local SubAgent-to-SubAgent spawn is denied (D1): spawning lineage is at depth {depth}, \
         only a parent at depth 0 may spawn a v1 child"
    )]
    DepthDenied { depth: u32 },
    #[error(
        "v1 profile {profile_id:?} declares tool {first:?}; V1 reasoning runs execute no tools"
    )]
    NonEmptyToolPolicy { profile_id: String, first: String },
}

/// The admitted, immutable run. Constructible only via
/// [`SubAgentRunV1::from_profile`].
pub struct SubAgentRunV1 {
    profile: SubAgentProfileV1,
    pinned_digest: String,
    run_ref: SubAgentRunRef,
    binding: OpaqueModelBinding,
}

impl std::fmt::Debug for SubAgentRunV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: identity and pinned digest only.
        f.debug_struct("SubAgentRunV1")
            .field("run_ref", &self.run_ref)
            .field("pinned_digest", &self.pinned_digest)
            .finish_non_exhaustive()
    }
}

impl SubAgentRunV1 {
    /// Admit a bounded V1 reasoning run from a digest-verified profile.
    /// Refuses any spawning lineage deeper than the parent (D1: children of
    /// V1 cannot spawn children) and any profile declaring child tools.
    pub fn from_profile(
        profile: SubAgentProfileV1,
        spawning_lineage: &LineageRef,
        binding: OpaqueModelBinding,
    ) -> Result<Self, RunAdmissionError> {
        if let Err(mismatch) = profile.verify_digest() {
            return Err(RunAdmissionError::ProfileDigest(mismatch.to_string()));
        }
        if spawning_lineage.depth() > 0 {
            return Err(RunAdmissionError::DepthDenied {
                depth: spawning_lineage.depth(),
            });
        }
        if let Some(first) = profile.tool_policy.tools.first() {
            return Err(RunAdmissionError::NonEmptyToolPolicy {
                profile_id: profile.profile_id,
                first: first.as_str().to_string(),
            });
        }
        Ok(Self {
            pinned_digest: profile.digest.clone(),
            run_ref: SubAgentRunRef::from_opaque(format!("subagent-v1-{}", uuid::Uuid::new_v4())),
            profile,
            binding,
        })
    }

    #[must_use]
    pub fn pinned_digest(&self) -> &str {
        &self.pinned_digest
    }

    /// The authority-minted, run-scoped identity (SA-13).
    #[must_use]
    pub fn run_ref(&self) -> &SubAgentRunRef {
        &self.run_ref
    }

    /// Drive the bounded run and return the terminal [`SubAgentReportV1`].
    pub async fn execute(self, ctx: SubAgentExecutionContextV1) -> SubAgentReportV1 {
        self.execute_inner(ctx).await
    }

    async fn execute_inner(self, ctx: SubAgentExecutionContextV1) -> SubAgentReportV1 {
        let SubAgentExecutionContextV1 {
            objective,
            bundle,
            lineage,
            budget_meter,
        } = ctx;

        let failure = |status: SubAgentTerminalFact, summary: String, usage: SubAgentUsage| {
            SubAgentReportV1 {
                run_ref: self.run_ref.clone(),
                profile_ref: VersionedProfileRef {
                    profile_id: self.profile.profile_id.clone(),
                    revision: self.profile.revision,
                    digest: self.pinned_digest.clone(),
                },
                context_bundle_ref: bundle.bundle_id().to_string(),
                status,
                summary,
                findings: Vec::new(),
                evidence_refs: Vec::new(),
                uncertainty: Vec::new(),
                recommendations: Vec::new(),
                requested_parent_actions: Vec::new(),
                proposed_candidates: Vec::new(),
                usage,
            }
        };

        // SA-18: the bundle digest is verified at admission AND before
        // use — mid-run bundle mutation is refused.
        if let Err(mismatch) = bundle.verify_digest() {
            return failure(
                SubAgentTerminalFact::Failed,
                format!("context bundle digest refused: {mismatch}"),
                budget_meter.usage(),
            );
        }

        // SA-5/SA-14/SA-15/SA-18/SA-19: the policy-enforced projection
        // over the ADMITTED bundle. Admission (the FIRST privacy
        // boundary) already rejected Private-Dyad/AgentSoul-derived refs
        // before the run could hold the bundle; what meets the profile
        // HERE is narrowing only.
        let projection = match bundle.projection_with_policy(
            &self.profile.context_policy.allowed_classes,
            &self.profile.privacy_policy.permitted_partitions,
            self.profile.context_policy.max_projection_bytes,
        ) {
            Ok(projection) => projection,
            Err(error) => {
                return failure(
                    SubAgentTerminalFact::Failed,
                    format!("context bundle refused by profile policy: {error}"),
                    budget_meter.usage(),
                );
            }
        };

        // SA-27: the action ceiling enforces BEFORE the bounded unit.
        if !budget_meter.try_record_action() {
            return failure(
                SubAgentTerminalFact::TimedOut,
                "action budget exhausted before the bounded unit".into(),
                budget_meter.usage(),
            );
        }

        let (system, user) = assemble_bounded_prompt(&objective, &projection, &lineage);
        let request = BoundedModelRequest {
            system,
            user,
            temperature: self.profile.model_policy.temperature,
        };

        let remaining = budget_meter.remaining_time();
        let outcome = match tokio::time::timeout(remaining, self.binding.complete(request)).await {
            Err(_) => {
                return failure(
                    SubAgentTerminalFact::TimedOut,
                    "time budget exhausted during the bounded unit".into(),
                    budget_meter.usage(),
                );
            }
            Ok(Err(e)) => {
                return failure(
                    SubAgentTerminalFact::Failed,
                    format!("model call failed: {e}"),
                    budget_meter.usage(),
                );
            }
            Ok(Ok(response)) => response,
        };

        // SA-27: token usage is recorded and enforced over counted usage.
        let tokens_exceeded = !budget_meter.record_tokens(outcome.tokens_in, outcome.tokens_out);

        let core = parse_report_core(&outcome.text);
        let usage = budget_meter.usage();
        let report = match core {
            Ok(core) => {
                let mut r = failure(SubAgentTerminalFact::Completed, core.summary, usage);
                r.findings = core.findings;
                r.evidence_refs = core.evidence_refs;
                r.uncertainty = core.uncertainty;
                r.recommendations = core.recommendations;
                r.requested_parent_actions = core.requested_parent_actions;
                r.proposed_candidates = core.proposed_candidates;
                r
            }
            Err(e) => failure(
                SubAgentTerminalFact::Failed,
                format!("report parse failed: {e}"),
                usage,
            ),
        };

        if tokens_exceeded {
            let mut timed_out = report;
            timed_out.status = SubAgentTerminalFact::TimedOut;
            timed_out.summary = format!(
                "token budget exhausted; last unit summary: {}",
                timed_out.summary
            );
            timed_out
        } else {
            report
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Execution context (four bounded inputs)
// ─────────────────────────────────────────────────────────────────────────

/// Hard bound on the objective (bounded text).
pub const MAX_OBJECTIVE_BYTES: usize = 8192;

#[derive(Debug, Clone)]
pub struct ObjectiveV1(String);

#[derive(Debug, thiserror::Error)]
#[error("objective exceeds the {MAX_OBJECTIVE_BYTES}-byte bound")]
pub struct ObjectiveTooLarge;

impl ObjectiveV1 {
    pub fn new(text: impl Into<String>) -> Result<Self, ObjectiveTooLarge> {
        let text = text.into();
        if text.len() > MAX_OBJECTIVE_BYTES {
            return Err(ObjectiveTooLarge);
        }
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The execution context: ONLY the four bounded inputs — objective,
/// ADMITTED bundle (the SA-14/SA-15 structural boundary: child execution
/// accepts only [`AdmittedContextBundleV1`], whose partition enum cannot
/// express Private Dyad or Agent Soul), lineage ref, shared budget meter —
/// and nothing else. No `Config`, no tool registry, no channel map, no
/// memory backend.
pub struct SubAgentExecutionContextV1 {
    objective: ObjectiveV1,
    bundle: AdmittedContextBundleV1,
    lineage: LineageRef,
    budget_meter: Arc<SubAgentBudgetMeter>,
}

impl SubAgentExecutionContextV1 {
    #[must_use]
    pub fn new(
        objective: ObjectiveV1,
        bundle: AdmittedContextBundleV1,
        lineage: LineageRef,
        budget_meter: Arc<SubAgentBudgetMeter>,
    ) -> Self {
        Self {
            objective,
            bundle,
            lineage,
            budget_meter,
        }
    }

    /// Typed inventory of everything the child context holds.
    #[must_use]
    pub fn inventory(&self) -> ContextInventory {
        ContextInventory {
            objective_bytes: self.objective.as_str().len(),
            bundle_id: self.bundle.bundle_id().to_string(),
            bundle_digest: self.bundle.digest().to_string(),
            lineage_root: self.lineage.root_ref().as_str().to_string(),
            lineage_depth: self.lineage.depth(),
            budget_max_actions: self.budget_meter.budget().max_actions,
        }
    }
}

/// What a child context contains. No credential, channel-map, memory, or
/// workspace fields exist on this type.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContextInventory {
    pub objective_bytes: usize,
    pub bundle_id: String,
    pub bundle_digest: String,
    pub lineage_root: String,
    pub lineage_depth: u32,
    pub budget_max_actions: u32,
}

// ─────────────────────────────────────────────────────────────────────────
// Prompt assembly (SA-16/SA-20)
// ─────────────────────────────────────────────────────────────────────────

/// Assemble the bounded child input from the objective and the bundle
/// projection ONLY. The parent transcript is structurally absent (no
/// parameter carries it — SA-16), and persona files reach a child only
/// via bundle refs, never the parent's (SA-20): this function performs
/// no file I/O at all.
fn assemble_bounded_prompt(
    objective: &ObjectiveV1,
    projection: &reiclaw_api::subagent_v1::BundleProjection,
    lineage: &LineageRef,
) -> (String, String) {
    let system = "You are a bounded reasoning SubAgent (v1). You receive one objective and an \
         immutable, digest-bound context snapshot. You have no tools. Respond with a \
         single JSON object and nothing else, matching exactly:\n\
         {\"summary\": string, \"findings\": [{\"finding_id\": string, \"statement\": \
         string, \"evidence_refs\": [string]}], \"evidence_refs\": [string], \"uncertainty\": [{\"uncertainty_id\": \
         string, \"topic_code\": string, \"impact\": string}], \"recommendations\": \
         [{\"recommendation_id\": string, \"statement\": string, \"evidence_refs\": \
         [string]}], \"requested_parent_actions\": [{\"action\": \
         \"ask_user\"|\"review_candidate\"|\"note\", \"subject_ref\": string}], \
         \"proposed_candidates\": [{\"candidate_id\": string, \"kind\": \
         \"ordinary_memory\"|\"user_model\"|\"agent_soul\"|\"skill\"|\"procedure\"|\
         \"private_dyad_derived\", \"content_digest\": string}]}\n\
         All arrays are optional and may be empty. Do not include chain-of-thought; \
         the report carries conclusions and evidence pointers only."
        .to_string();
    let mut user = format!("[Objective]\n{}\n", objective.as_str());
    user.push_str(&format!(
        "[Lineage]\nroot={} depth={}\n",
        lineage.root_ref().as_str(),
        lineage.depth()
    ));
    // Child-visible digest is the PROJECTION digest (existence-blind —
    // SA-14.3); the pinned full-bundle digest never reaches the child.
    user.push_str(&format!(
        "[ContextBundle {} projection_digest {}]\n",
        projection.bundle_id, projection.projection_digest
    ));
    if !projection.objective_context.is_empty() {
        user.push_str(&format!(
            "[ObjectiveContext]\n{}\n",
            projection.objective_context
        ));
    }
    for source in &projection.source_refs {
        user.push_str(&format!(
            "[SourceRef {} partition {} digest {}]\n",
            source.ref_id, source.partition, source.content_digest
        ));
    }
    for fact in &projection.applicable_user_model {
        user.push_str(&format!(
            "[ProjectedFact {} digest {}]\n",
            fact.fact_id, fact.statement_digest
        ));
    }
    for skill in &projection.skill_refs {
        user.push_str(&format!("[SkillRef {skill}]\n"));
    }
    for procedure in &projection.procedure_refs {
        user.push_str(&format!("[ProcedureRef {procedure}]\n"));
    }
    (system, user)
}

/// The JSON core the model returns; the run wraps it into the frozen
/// report type. `deny_unknown_fields` makes the parser itself reject
/// smuggled extras (a `chain_of_thought` field or any other unlisted
/// key fails the parse → the run ends `failed`).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportCore {
    summary: String,
    #[serde(default)]
    findings: Vec<Finding>,
    #[serde(default)]
    evidence_refs: Vec<reiclaw_api::subagent_v1::EvidenceRef>,
    #[serde(default)]
    uncertainty: Vec<reiclaw_api::subagent_v1::UncertaintyItem>,
    #[serde(default)]
    recommendations: Vec<Recommendation>,
    #[serde(default)]
    requested_parent_actions: Vec<reiclaw_api::subagent_v1::RequestedParentAction>,
    #[serde(default)]
    proposed_candidates: Vec<ProposedCandidate>,
}

fn parse_report_core(text: &str) -> Result<ReportCore, anyhow::Error> {
    let trimmed = text.trim();
    // Tolerate a surrounding code fence, nothing else.
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let core: ReportCore = serde_json::from_str(body)?;
    Ok(core)
}

// ─────────────────────────────────────────────────────────────────────────
// Parent-side spawn tool
// ─────────────────────────────────────────────────────────────────────────

/// The Parent's bounded reasoning-child spawn tool. This is a PARENT
/// capability (D1: `Parent → local SubAgent` allowed): the child it
/// spawns is a V1 reasoning run with no tools, an immutable bundle, and
/// a structured report. Children cannot use this tool — run admission
/// refuses any spawning lineage at depth > 0 (SA-12).
pub struct ReasoningSubagentTool {
    config: Arc<Config>,
    parent_alias: String,
    security: Arc<reiclaw_policy::SecurityPolicy>,
    lineage: Option<LineageRef>,
    /// The agent's configured advisor (`advisor = "model:<type>.<alias>"`).
    /// When set, the child runs on this model instead of the parent's
    /// own, and calls count against the per-turn advisor budget.
    advisor: Option<AdvisorBinding>,
    #[cfg(test)]
    model_binding_override: Option<OpaqueModelBinding>,
    #[cfg(test)]
    profile_override: Option<SubAgentProfileV1>,
}

/// An advisor model the tool consults instead of the parent's provider.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AdvisorBinding {
    /// Dotted `type.alias` into `[providers.models]`.
    provider_ref: String,
    max_calls_per_turn: u32,
}

/// Tool description when no advisor is configured.
const REASONING_DESCRIPTION: &str = "Run one bounded reasoning SubAgent against a focused objective. The child \
     receives only the objective and a digest-bound context snapshot — no tools, \
     no conversation history, no memory — and returns a structured report \
     (summary, findings, evidence pointers, uncertainty, recommendations, \
     candidate changes for parent disposition). Use for focused analysis that \
     should not pollute this agent's history.";

/// Tool description when the agent has an advisor configured.
const ADVISOR_DESCRIPTION: &str = "Consult this agent's configured advisor model: a stronger model for hard \
     questions. Use it when a question is beyond what you can answer confidently. \
     The advisor sees ONLY the objective — no tools, no conversation history, no \
     memory — so write the objective as the question plus a short summary of the \
     facts needed to answer it (at most 8 KiB). It returns a structured report \
     (summary, findings, uncertainty, recommendations). Calls per turn are \
     limited.";

impl ReasoningSubagentTool {
    pub const NAME: &'static str = "reasoning_subagent";

    pub fn new(
        config: Arc<Config>,
        parent_alias: impl Into<String>,
        security: Arc<reiclaw_policy::SecurityPolicy>,
    ) -> Self {
        Self {
            config,
            parent_alias: parent_alias.into(),
            security,
            lineage: None,
            advisor: None,
            #[cfg(test)]
            model_binding_override: None,
            #[cfg(test)]
            profile_override: None,
        }
    }

    /// Bind the agent's advisor model (`type.alias` into
    /// `[providers.models]`): the child runs on it instead of the parent's
    /// provider, the description tells the model it is consulting an
    /// advisor, and calls are capped per turn (default
    /// [`reiclaw_config::advisor::DEFAULT_ADVISOR_MAX_CALLS_PER_TURN`]).
    #[must_use]
    pub fn with_advisor(mut self, provider_ref: impl Into<String>) -> Self {
        self.advisor = Some(AdvisorBinding {
            provider_ref: provider_ref.into(),
            max_calls_per_turn: reiclaw_config::advisor::DEFAULT_ADVISOR_MAX_CALLS_PER_TURN,
        });
        self
    }

    /// Override the per-turn advisor call cap. No effect without
    /// [`Self::with_advisor`].
    #[must_use]
    pub fn with_advisor_max_calls_per_turn(mut self, max_calls: u32) -> Self {
        if let Some(advisor) = self.advisor.as_mut() {
            advisor.max_calls_per_turn = max_calls;
        }
        self
    }

    /// The advisor's `type.alias`, when one is bound.
    #[must_use]
    pub fn advisor_ref(&self) -> Option<&str> {
        self.advisor
            .as_ref()
            .map(|advisor| advisor.provider_ref.as_str())
    }

    /// Carry the spawning context's lineage (SA-9): the tool refuses to
    /// spawn when its own context is already a child (admission-side
    /// D1 enforcement via `SubAgentRunV1::from_profile`).
    #[must_use]
    pub fn with_lineage(mut self, lineage: Option<LineageRef>) -> Self {
        self.lineage = lineage;
        self
    }

    /// The spawn lineage this tool instance carries (SA-9). Registry
    /// builders MUST thread the run's lineage here; a nested registry
    /// whose reasoning tool reads `None` would admit D1-forbidden
    /// spawns from depth > 0 contexts. Public so the registry-wiring
    /// discrimination test can assert the thread-through.
    #[must_use]
    pub fn carried_lineage(&self) -> Option<&LineageRef> {
        self.lineage.as_ref()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_model_stub(mut self, stub: Arc<tests::StubResolver>) -> Self {
        self.model_binding_override = Some(OpaqueModelBinding::from_stub(stub));
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_profile_override(mut self, profile: SubAgentProfileV1) -> Self {
        self.profile_override = Some(profile);
        self
    }

    fn effective_lineage(&self) -> LineageRef {
        self.lineage.clone().unwrap_or_else(|| {
            LineageRef::new_root(ParentRunRef::from_opaque(format!(
                "agent:{}",
                self.parent_alias
            )))
        })
    }

    /// The model policy the child runs under, in precedence order: the
    /// profile's own pin (part of its digest-bound contract), then the
    /// agent's advisor, then the parent's own resolved provider.
    fn resolve_model_policy(&self, profile: &SubAgentProfileV1) -> Result<ModelPolicyV1, String> {
        let policy = if !profile.model_policy.provider_ref.trim().is_empty() {
            profile.model_policy.clone()
        } else if let Some(advisor) = &self.advisor {
            // The advisor alias pins its own model; the binding reads it
            // from `[providers.models.<type>.<alias>]` at use time.
            ModelPolicyV1 {
                provider_ref: advisor.provider_ref.clone(),
                model: None,
                temperature: None,
            }
        } else {
            let (provider_ref, model) = self
                .config
                .resolved_model_provider_for_agent(&self.parent_alias)
                .map(|(family, alias, entry)| (format!("{family}.{alias}"), entry.model.clone()))
                .unwrap_or_default();
            ModelPolicyV1 {
                provider_ref,
                model,
                temperature: None,
            }
        };
        if policy.provider_ref.trim().is_empty() {
            return Err(
                "no model provider resolvable for the reasoning child (neither the \
                 profile's model_policy nor the parent's provider configuration)"
                    .to_string(),
            );
        }
        Ok(policy)
    }

    /// Run one bounded child. `meter_override` is None on the production
    /// path (a fresh per-run meter is minted from the profile); tests may
    /// pass an explicit shared meter to prove the SA-8 sharing semantics.
    async fn run_child(&self, objective: &str) -> Result<SubAgentReportV1, String> {
        self.run_child_with(objective, None).await
    }

    pub(crate) async fn run_child_with(
        &self,
        objective: &str,
        meter_override: Option<Arc<SubAgentBudgetMeter>>,
    ) -> Result<SubAgentReportV1, String> {
        let lineage = self.effective_lineage();
        #[cfg(test)]
        let profile = self
            .profile_override
            .clone()
            .unwrap_or_else(default_reasoning_profile);
        #[cfg(not(test))]
        let profile = default_reasoning_profile();

        // SA-8, per-run scope: a FRESH meter is minted for THIS run and
        // shared between this parent-side admission and the child
        // execution (the same `Arc` crosses the boundary as the budget
        // input); it is discarded at the run's terminal.
        let meter = match meter_override {
            Some(meter) => meter,
            None => Arc::new(SubAgentBudgetMeter::new(profile.budget)),
        };
        if meter.exhausted() {
            return Err(
                "reasoning_subagent: the budget meter for this run is exhausted before \
                 start (shared-override meters only); a fresh production run mints a \
                 fresh meter"
                    .to_string(),
            );
        }

        // The model binding is host-resolved from the profile's
        // model_policy at use time; when the profile names no provider,
        // the agent's advisor, else the parent's own resolved provider, is
        // used (still opaque to the child). See `resolve_model_policy`.
        #[cfg(test)]
        let binding = match self.model_binding_override.clone() {
            Some(binding) => binding,
            None => OpaqueModelBinding::from_config(
                Arc::clone(&self.config),
                self.resolve_model_policy(&profile)?,
            ),
        };
        #[cfg(not(test))]
        let binding = OpaqueModelBinding::from_config(
            Arc::clone(&self.config),
            self.resolve_model_policy(&profile)?,
        );

        // SA-18/SA-16: a minimal, digest-bound bundle. The objective is
        // the context; the parent transcript is excluded by default
        // (auditable exclusion, SA-19); no memory read happens on the
        // V1 path (D2 snapshot-only).
        let objective = ObjectiveV1::new(objective).map_err(|e| e.to_string())?;
        let mut bundle = ContextBundleV1 {
            bundle_id: format!("bundle-{}", uuid::Uuid::new_v4()),
            revision: 1,
            digest: String::new(),
            parent_ref: ParentRunRef::from_opaque(lineage.root_ref().as_str()),
            objective_context: objective.as_str().to_string(),
            source_refs: Vec::new(),
            applicable_user_model: Vec::new(),
            skill_refs: Vec::new(),
            procedure_refs: Vec::new(),
            explicit_exclusions: vec![ContextClassV1::ParentTranscript],
            redaction_policy: BundleRedactionPolicy::default(),
        };
        bundle.digest = bundle.compute_digest();

        // The FIRST privacy boundary (SA-14/SA-15): reject — never
        // filter — Private-Dyad/AgentSoul-derived refs before the run
        // exists.
        let bundle = bundle.admit().map_err(|error| {
            format!("context bundle refused at the admitted-bundle boundary: {error}")
        })?;

        let run = SubAgentRunV1::from_profile(profile, &lineage, binding)
            .map_err(|e| format!("run admission refused: {e}"))?;
        let ctx = SubAgentExecutionContextV1::new(objective, bundle, lineage.child(), meter);

        Ok(run.execute(ctx).await)
    }
}

#[async_trait]
impl Tool for ReasoningSubagentTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        if self.advisor.is_some() {
            ADVISOR_DESCRIPTION
        } else {
            REASONING_DESCRIPTION
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The bounded, self-contained question or analysis task. The child does not see this conversation's history."
                }
            },
            "required": ["objective"]
        })
    }

    /// The structured output IS the child's `SubAgentReportV1` (SA-21):
    /// the report travels as `ToolOutput.data`; the display text is
    /// presentation derived from it, never the contract.
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "description": "SubAgentReportV1 — the ONLY child→parent result channel",
            "required": ["run_ref", "status", "summary", "usage"],
            "properties": {
                "run_ref": {"type": "string"},
                "profile_ref": {"type": "object"},
                "context_bundle_ref": {"type": "string"},
                "status": {
                    "type": "string",
                    "enum": ["stopped", "aborted", "timed_out", "completed", "failed"]
                },
                "summary": {"type": "string"},
                "findings": {"type": "array"},
                "evidence_refs": {"type": "array"},
                "uncertainty": {"type": "array"},
                "recommendations": {"type": "array"},
                "requested_parent_actions": {"type": "array"},
                "proposed_candidates": {"type": "array"},
                "usage": {
                    "type": "object",
                    "required": ["elapsed_ms", "tokens_in", "tokens_out", "actions"]
                }
            },
            "additionalProperties": false
        }))
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Card / risk-profile self-check:
        // the tool must be granted by the governing card or listed in the
        // risk profile's allowed_tools (when a list exists), and not
        // excluded.
        if let Some(card) = self.config.card_for_agent(&self.parent_alias) {
            let granted = card.grants.tools.iter().any(|g| g.tool == Self::NAME);
            if !granted {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "reasoning_subagent: refused — card governing agent '{}' does not grant reasoning_subagent",
                        self.parent_alias
                    )),
                });
            }
        } else if let Some(rp) = self.config.risk_profile_for_agent(&self.parent_alias) {
            let excluded = rp.excluded_tools.iter().any(|t| t == Self::NAME);
            let allowed_when_listed = match &rp.allowed_tools {
                None => true,
                Some(tools) => tools.iter().any(|t| t == Self::NAME),
            };
            if excluded || !allowed_when_listed {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "reasoning_subagent: refused — agent '{}' risk_profile does not list reasoning_subagent in allowed_tools",
                        self.parent_alias
                    )),
                });
            }
        }

        if let Err(error) = self
            .security
            .enforce_tool_operation(crate::security::policy::ToolOperation::Act, Self::NAME)
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        let objective = match args
            .get("objective")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => value.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing or empty 'objective' parameter".into()),
                });
            }
        };

        // The objective is the only thing the child (and, with an advisor,
        // the advisor's vendor) sees. Refuse oversized ones up front with a
        // message the model can act on, before any budget is spent.
        if objective.len() > MAX_OBJECTIVE_BYTES {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "reasoning_subagent: objective is {} bytes, over the {MAX_OBJECTIVE_BYTES}-byte \
                     (8 KiB) limit; send the question plus a short summary of the facts \
                     needed, not whole documents or transcripts",
                    objective.len()
                )),
            });
        }

        if let Some(advisor) = &self.advisor {
            let error = match try_reserve_advisor_call(advisor.max_calls_per_turn) {
                AdvisorReservation::Reserved => None,
                AdvisorReservation::BudgetUsed => Some(format!(
                    "reasoning_subagent: advisor budget used — this turn already consulted \
                     the advisor {} time(s), the per-turn limit; continue without the \
                     advisor or answer with what you have",
                    advisor.max_calls_per_turn
                )),
                AdvisorReservation::NoTurnScope => {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                            .with_category(::reiclaw_log::EventCategory::Tool)
                            .with_outcome(::reiclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "tool": Self::NAME,
                                "advisor": advisor.provider_ref,
                            })),
                        "advisor call refused: no agent turn scope, so the per-turn cap cannot be enforced"
                    );
                    Some(
                        "reasoning_subagent: the advisor can only be consulted from within an \
                         agent turn (no per-turn advisor budget is in scope here); continue \
                         without the advisor"
                            .to_string(),
                    )
                }
            };
            if let Some(error) = error {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(error),
                });
            }
        }

        match self.run_child(&objective).await {
            Ok(report) => {
                let advisor_note = self
                    .advisor_ref()
                    .map(|provider_ref| format!(" advisor {provider_ref}"))
                    .unwrap_or_default();
                let mut summary = format!(
                    "reasoning_subagent {} [{}]{advisor_note}\nsummary: {}",
                    report.run_ref.as_str(),
                    serde_json::to_value(report.status)
                        .unwrap_or_default()
                        .as_str()
                        .unwrap_or("unknown"),
                    report.summary,
                );
                for finding in &report.findings {
                    summary.push_str(&format!(
                        "\nfinding {}: {}",
                        finding.finding_id, finding.statement
                    ));
                }
                if !report.uncertainty.is_empty() {
                    summary.push_str(&format!(
                        "\nuncertainty items: {}",
                        report
                            .uncertainty
                            .iter()
                            .map(|u| u.uncertainty_id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !report.proposed_candidates.is_empty() {
                    summary.push_str(&format!(
                        "\nproposed candidates for parent disposition: {}",
                        report
                            .proposed_candidates
                            .iter()
                            .map(|c| c.candidate_id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let success = report.status == SubAgentTerminalFact::Completed;
                // SA-21: the structured report IS the result — the full
                // SubAgentReportV1 travels as `ToolOutput.data`, AND the
                // rendered text carries the report JSON verbatim, so the
                // structured result survives end to end.
                let report_json = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
                let json_text = serde_json::to_string_pretty(&report_json)
                    .unwrap_or_else(|_| report_json.to_string());
                let rendered = format!("{summary}\n\n[SubAgentReportV1]\n{json_text}");
                // SA-21 end to end: on a non-Completed terminal fact the
                // common dispatcher discards `output`/`data` and shows
                // the parent model only the error string — so the error
                // string carries the structured report too.
                let error = (!success).then(|| {
                    format!(
                        "child ended {:?}{advisor_note}\n[SubAgentReportV1]\n{json_text}",
                        report.status
                    )
                });
                Ok(ToolResult {
                    success,
                    output: ToolOutput::json_with_text(report_json, rendered),
                    error,
                })
            }
            Err(error) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(match self.advisor_ref() {
                    Some(provider_ref) => format!("advisor {provider_ref}: {error}"),
                    None => error,
                }),
            }),
        }
    }
}

#[cfg(test)]
mod tests;
