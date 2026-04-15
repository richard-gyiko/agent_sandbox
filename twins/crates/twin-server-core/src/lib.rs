//! Generic control surface for digital-twin servers.
//!
//! This crate provides the shared infrastructure that every twin server
//! needs: control routes (reset, snapshot, restore, events), a scenario
//! engine, run storage, fault injection, and the data types that glue
//! them together.  All logic is generic over `T: TwinService`.

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    collections::BTreeSet,
    collections::HashMap,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Component, Path as FsPath, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    sync::Mutex,
    time::{Duration, sleep},
};
use twin_kernel::{TwinConfig, TwinEvent, TwinEventContext};
use twin_scenario::{
    FaultEffect, FaultRule, ScenarioDocument, parse_scenario_json,
};
use twin_service::{
    AssertionResult, DiscoveryMeta, DiscoveryMethod, DiscoveryResource,
    ResolvedActorId, SharedTwinState, TwinRuntime, TwinService,
};

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// Standard JSON API error type used throughout the control surface.
pub type ApiError = (StatusCode, Json<serde_json::Value>);

/// Convenience result type wrapping `Json<T>` for success.
pub type ApiResult<T> = Result<Json<T>, ApiError>;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn generate_request_id() -> String {
    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("twin-req-{id}")
}

fn extract_trace_id(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("X-Trace-Id").and_then(|v| v.to_str().ok()) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // W3C traceparent: version-traceid-parentid-flags
    if let Some(v) = headers.get("traceparent").and_then(|v| v.to_str().ok()) {
        let mut parts = v.split('-');
        let _version = parts.next();
        if let Some(trace_id) = parts.next() {
            let trace_id = trace_id.trim();
            if !trace_id.is_empty() {
                return Some(trace_id.to_string());
            }
        }
    }
    None
}

fn seed_error_json(code: &str, message: &str, details: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": code,
            "message": message,
            "details": details,
        }
    })
}

fn format_seed_json_error(err: serde_json::Error) -> serde_json::Value {
    seed_error_json(
        "invalid_seed_json",
        "seed payload must be valid JSON",
        vec![serde_json::json!({
            "path": "$",
            "line": err.line(),
            "column": err.column(),
            "message": err.to_string(),
        })],
    )
}

fn format_seed_service_error(err: &str) -> serde_json::Value {
    // Standardized format expected from twin services:
    // "invalid seed at <path>: <message>"
    if let Some(rest) = err.strip_prefix("invalid seed at ") {
        if let Some((path, message)) = rest.split_once(':') {
            return seed_error_json(
                "invalid_seed_payload",
                "seed payload validation failed",
                vec![serde_json::json!({
                    "path": path.trim(),
                    "message": message.trim(),
                })],
            );
        }
    }

    seed_error_json(
        "seed_failed",
        "seed payload validation failed",
        vec![serde_json::json!({
            "path": "$",
            "message": err,
        })],
    )
}

// ---------------------------------------------------------------------------
// Shared application state (generic over T)
// ---------------------------------------------------------------------------

/// Server-wide state shared across all Axum handlers.
///
/// `Clone` is implemented manually so that `T` does not need to be `Clone`
/// (all fields are behind `Arc`).
pub struct AppState<T: TwinService> {
    pub runtime: Arc<Mutex<TwinRuntime<T>>>,
    pub scenario: Arc<Mutex<ScenarioRuntime>>,
    pub run_store: Arc<Mutex<RunStore>>,
    pub session_store: Arc<Mutex<SessionStore>>,
    pub scenarios_dir: PathBuf,
}

impl<T: TwinService> Clone for AppState<T> {
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime.clone(),
            scenario: self.scenario.clone(),
            run_store: self.run_store.clone(),
            session_store: self.session_store.clone(),
            scenarios_dir: self.scenarios_dir.clone(),
        }
    }
}

/// Runtime state for the scenario engine (faults, run counter, in-memory runs).
pub struct ScenarioRuntime {
    pub faults: Vec<FaultRule>,
    pub decision_index: u64,
    pub next_run_id: u64,
    pub runs: BTreeMap<String, StoredRun>,
}

impl ScenarioRuntime {
    pub fn new(next_run_id: u64) -> Self {
        Self {
            faults: Vec::new(),
            decision_index: 0,
            next_run_id,
            runs: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Outcome of a single timeline event during scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineOutcome {
    pub at_ms: u64,
    pub actor_id: String,
    pub action_type: String,
    pub endpoint: String,
    pub outcome: String,
    pub error: Option<String>,
}

/// Record of a fault that fired during scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultHit {
    pub fault_id: String,
    pub endpoint: String,
    pub actor_id: Option<String>,
    pub effect_type: String,
}

/// Full report produced after a scenario run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioRunReport {
    pub run_id: String,
    pub source_run_id: Option<String>,
    pub scenario_name: String,
    pub status: String,
    pub seed: u64,
    pub start_time_unix_ms: i64,
    pub events_applied: usize,
    pub timeline_outcomes: Vec<TimelineOutcome>,
    pub fault_hits: Vec<FaultHit>,
    pub assertions: Vec<AssertionResult>,
    pub failed_assertions: usize,
    pub final_snapshot_revision: u64,
    pub final_snapshot_hash: String,
    pub error: Option<String>,
}

/// In-memory record of a completed run (report + original scenario).
#[derive(Debug, Clone)]
pub struct StoredRun {
    pub report: ScenarioRunReport,
    pub scenario: ScenarioDocument,
}

/// Full artifact persisted to disk (report + scenario + kernel events + snapshot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunArtifact {
    pub report: ScenarioRunReport,
    pub scenario: ScenarioDocument,
    pub events: Vec<TwinEvent>,
    pub snapshot: twin_kernel::TwinState,
}

/// Summary item returned by the run-list endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunListItem {
    pub run_id: String,
    pub scenario_name: String,
    pub status: String,
    pub seed: u64,
    pub start_time_unix_ms: i64,
    pub final_snapshot_hash: String,
}

/// Result of diffing two run artifacts.
#[derive(Debug, Serialize)]
pub struct RunDiffResult {
    pub run_id_a: String,
    pub run_id_b: String,
    pub status_a: String,
    pub status_b: String,
    pub status_equal: bool,
    pub snapshot_hash_a: String,
    pub snapshot_hash_b: String,
    pub snapshot_hash_equal: bool,
    pub events_count_a: usize,
    pub events_count_b: usize,
    pub events_count_equal: bool,
    pub assertions_changed: Vec<String>,
    pub timeline_mismatch_indices: Vec<usize>,
    pub fault_hit_count_a: usize,
    pub fault_hit_count_b: usize,
    pub fault_hit_count_equal: bool,
}

/// Structural validation result for a scenario document.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------

/// Status of a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Ended,
}

/// Metadata for a single session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub status: SessionStatus,
    pub created_at_unix_ms: i64,
    pub ended_at_unix_ms: Option<i64>,
    pub final_event_count: usize,
    /// Snapshot captured at session end (if ended).
    #[serde(skip)]
    pub final_snapshot: Option<twin_kernel::TwinState>,
    /// Events captured at session end (if ended).
    #[serde(skip)]
    pub frozen_events: Option<Vec<TwinEvent>>,
}

/// In-memory session store.
pub struct SessionStore {
    sessions: BTreeMap<String, SessionInfo>,
    next_session_id: u64,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            next_session_id: 1,
        }
    }

    pub fn create(&mut self, name: Option<String>, created_at_unix_ms: i64) -> String {
        let session_id = format!("sess_{:06}", self.next_session_id);
        self.next_session_id += 1;
        self.sessions.insert(
            session_id.clone(),
            SessionInfo {
                session_id: session_id.clone(),
                name,
                status: SessionStatus::Active,
                created_at_unix_ms,
                ended_at_unix_ms: None,
                final_event_count: 0,
                final_snapshot: None,
                frozen_events: None,
            },
        );
        session_id
    }

    /// Find the currently active session, if any.
    pub fn active_session(&self) -> Option<&SessionInfo> {
        self.sessions.values().find(|s| s.status == SessionStatus::Active)
    }

    pub fn get(&self, session_id: &str) -> Option<&SessionInfo> {
        self.sessions.get(session_id)
    }

    pub fn get_mut(&mut self, session_id: &str) -> Option<&mut SessionInfo> {
        self.sessions.get_mut(session_id)
    }

    pub fn list(&self) -> Vec<&SessionInfo> {
        self.sessions.values().collect()
    }
}

// ---------------------------------------------------------------------------
// Request types (for control endpoints)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EventQueryParams {
    pub actor_id: Option<String>,
    pub endpoint: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub session_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ReplayRequest {
    pub run_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RunListQuery {
    pub status: Option<String>,
    pub scenario_name: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyFileRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct RunDiffRequest {
    pub run_id_a: String,
    pub run_id_b: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub name: Option<String>,
    pub seed: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Run store (disk persistence)
// ---------------------------------------------------------------------------

/// Simple file-system backed store for run artifacts.
pub struct RunStore {
    base_dir: PathBuf,
    index_path: PathBuf,
}

impl RunStore {
    pub fn new(base_dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&base_dir)
            .map_err(|e| format!("failed to create run dir: {e}"))?;
        let index_path = base_dir.join("index.json");
        if !index_path.exists() {
            std::fs::write(&index_path, "[]")
                .map_err(|e| format!("failed to initialize run index: {e}"))?;
        }
        Ok(Self {
            base_dir,
            index_path,
        })
    }

    pub fn next_run_id(&self) -> u64 {
        let ids = self.read_index();
        ids.into_iter()
            .filter_map(|id| id.strip_prefix("run_").and_then(|n| n.parse::<u64>().ok()))
            .max()
            .unwrap_or(0)
    }

    pub fn persist_run(&self, artifact: &RunArtifact) -> Result<(), String> {
        let run_path = self.base_dir.join(format!("{}.json", artifact.report.run_id));
        let payload = serde_json::to_vec_pretty(artifact)
            .map_err(|e| format!("failed to encode run artifact: {e}"))?;
        std::fs::write(&run_path, payload)
            .map_err(|e| format!("failed to write run artifact: {e}"))?;

        let mut ids = self.read_index();
        if !ids.iter().any(|id| id == &artifact.report.run_id) {
            ids.push(artifact.report.run_id.clone());
            let index_payload = serde_json::to_vec_pretty(&ids)
                .map_err(|e| format!("failed to encode run index: {e}"))?;
            std::fs::write(&self.index_path, index_payload)
                .map_err(|e| format!("failed to write run index: {e}"))?;
        }
        Ok(())
    }

    pub fn load_run(&self, run_id: &str) -> Result<RunArtifact, String> {
        if !is_safe_token(run_id) {
            return Err("invalid run id".to_string());
        }
        let run_path = self.base_dir.join(format!("{run_id}.json"));
        let raw = std::fs::read_to_string(&run_path)
            .map_err(|e| format!("failed to read run artifact: {e}"))?;
        serde_json::from_str(&raw).map_err(|e| format!("failed to parse run artifact: {e}"))
    }

    pub fn list_run_reports(&self) -> Vec<ScenarioRunReport> {
        self.read_index()
            .into_iter()
            .filter_map(|id| self.load_run(&id).ok().map(|artifact| artifact.report))
            .collect()
    }

    pub fn read_index(&self) -> Vec<String> {
        let raw = std::fs::read_to_string(&self.index_path).unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Fault injection
// ---------------------------------------------------------------------------

/// Evaluate fault rules for the given endpoint/actor.  Returns the first
/// matching `FaultRule` whose probability fires, or `None`.
pub async fn evaluate_fault<T: TwinService>(
    state: &AppState<T>,
    endpoint: &str,
    actor_id: Option<&str>,
) -> Option<FaultRule> {
    let seed = {
        let rt = state.runtime.lock().await;
        rt.kernel.config().seed
    };

    let mut scenario_runtime = state.scenario.lock().await;
    scenario_runtime.decision_index += 1;
    let decision_index = scenario_runtime.decision_index;

    for rule in &scenario_runtime.faults {
        if rule.when.endpoint != endpoint {
            continue;
        }
        if let Some(expected_actor) = &rule.when.actor_id {
            if actor_id != Some(expected_actor.as_str()) {
                continue;
            }
        }
        let p = rule.when.probability.clamp(0.0, 1.0) as f64;
        if p <= 0.0 {
            continue;
        }
        let value = deterministic_probability(
            seed,
            decision_index,
            endpoint,
            actor_id.unwrap_or(""),
            &rule.id,
        );
        if value <= p {
            return Some(rule.clone());
        }
    }
    None
}

/// Deterministic, hash-based probability in `[0, 1)`.
pub fn deterministic_probability(
    seed: u64,
    decision_index: u64,
    endpoint: &str,
    actor_id: &str,
    fault_id: &str,
) -> f64 {
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    decision_index.hash(&mut hasher);
    endpoint.hash(&mut hasher);
    actor_id.hash(&mut hasher);
    fault_id.hash(&mut hasher);
    let n = hasher.finish() % 10_000;
    n as f64 / 10_000.0
}

// ---------------------------------------------------------------------------
// Scenario engine
// ---------------------------------------------------------------------------

/// Execute a full scenario run.
///
/// Resets the kernel, seeds initial state via `T::seed_from_scenario()`,
/// replays timeline events (delegating to `T::execute_timeline_action()`),
/// evaluates assertions via `T::evaluate_assertion()`, and persists the
/// run artifact.
pub async fn run_scenario<T: TwinService>(
    state: &AppState<T>,
    scenario: ScenarioDocument,
    source_run_id: Option<String>,
) -> (ScenarioRunReport, Option<ApiError>) {
    let run_id = {
        let mut scenario_runtime = state.scenario.lock().await;
        scenario_runtime.next_run_id += 1;
        format!("run_{:06}", scenario_runtime.next_run_id)
    };

    let mut api_error: Option<ApiError> = None;
    let mut timeline_outcomes = Vec::new();
    let mut fault_hits = Vec::new();
    let mut assertions = Vec::new();
    let mut failed_assertions = 0usize;
    let mut error_message: Option<String> = None;
    let mut status = "ok".to_string();
    let mut events_applied = 0usize;

    // --- reset + seed ---
    let init_result = {
        let mut rt = state.runtime.lock().await;
        rt.reset(TwinConfig {
            seed: scenario.seed,
            start_time_unix_ms: scenario.start_time_unix_ms,
        });
        rt.service
            .seed_from_scenario(&scenario.initial_state)
            .map_err(|e| map_error_string(&e.to_string()))
    };
    if let Err(err) = init_result {
        status = "error".to_string();
        error_message = Some("initial_state failed".to_string());
        api_error = Some(err);
    } else {
        // --- install fault rules ---
        {
            let mut scenario_runtime = state.scenario.lock().await;
            scenario_runtime.faults = scenario.faults.clone();
            scenario_runtime.decision_index = 0;
        }

        // --- execute timeline ---
        let mut events = scenario.timeline.clone();
        events.sort_by_key(|e| e.at_ms);
        for event in events {
            let action_json = event.action.clone();
            let action_type = action_type_from_json(&action_json).to_string();

            // Use the action type as the endpoint for pre-execution fault
            // matching.  The twin's `execute_timeline_action` returns the
            // real endpoint in `TimelineActionResult`; if faults need to
            // match on exact route paths the scenario should use the same
            // action-type string in the fault `when.endpoint` field.
            let fault_endpoint = action_type.clone();

            // --- fault evaluation ---
            let fault = evaluate_fault(state, &fault_endpoint, Some(&event.actor_id)).await;
            if let Some(effect) = &fault {
                match &effect.effect {
                    FaultEffect::Latency { delay_ms } => {
                        sleep(Duration::from_millis(*delay_ms)).await;
                    }
                    FaultEffect::HttpError { status: code, message } => {
                        let http_status =
                            StatusCode::from_u16(*code).unwrap_or(StatusCode::BAD_GATEWAY);
                        let hit = FaultHit {
                            fault_id: effect.id.clone(),
                            endpoint: fault_endpoint.clone(),
                            actor_id: Some(event.actor_id.clone()),
                            effect_type: "http_error".to_string(),
                        };
                        fault_hits.push(hit);
                        {
                            let mut rt = state.runtime.lock().await;
                            rt.kernel.record_event(
                                &fault_endpoint,
                                Some(event.actor_id.clone()),
                                "fault_http_error",
                                message.clone(),
                                Some(effect.id.clone()),
                            );
                        }
                        timeline_outcomes.push(TimelineOutcome {
                            at_ms: event.at_ms,
                            actor_id: event.actor_id,
                            action_type,
                            endpoint: fault_endpoint,
                            outcome: "error".to_string(),
                            error: Some(message.clone()),
                        });
                        status = "error".to_string();
                        error_message = Some(message.clone());
                        api_error = Some((
                            http_status,
                            Json(serde_json::json!({ "error": message })),
                        ));
                        break;
                    }
                }
            }

            // --- execute action via twin ---
            let exec_result = {
                let mut rt = state.runtime.lock().await;
                let result = rt
                    .service
                    .execute_timeline_action(&action_json, &event.actor_id);
                let record_endpoint = match &result {
                    Ok(action_result) => action_result.endpoint.clone(),
                    Err(_) => fault_endpoint.clone(),
                };
                match &result {
                    Ok(_) => {
                        rt.kernel.record_event(
                            &record_endpoint,
                            Some(event.actor_id.clone()),
                            "ok",
                            "timeline action applied",
                            fault.as_ref().map(|f| f.id.clone()),
                        );
                    }
                    Err(err) => {
                        rt.kernel.record_event(
                            &record_endpoint,
                            Some(event.actor_id.clone()),
                            "error",
                            err.to_string(),
                            fault.as_ref().map(|f| f.id.clone()),
                        );
                    }
                }
                result.map(|r| (r.endpoint.clone(), r))
            };

            match exec_result {
                Ok((endpoint, _action_result)) => {
                    events_applied += 1;
                    if let Some(f) = fault {
                        fault_hits.push(FaultHit {
                            fault_id: f.id,
                            endpoint: endpoint.clone(),
                            actor_id: Some(event.actor_id.clone()),
                            effect_type: "latency".to_string(),
                        });
                    }
                    timeline_outcomes.push(TimelineOutcome {
                        at_ms: event.at_ms,
                        actor_id: event.actor_id,
                        action_type,
                        endpoint,
                        outcome: "ok".to_string(),
                        error: None,
                    });
                }
                Err(err) => {
                    let err_message = err.to_string();
                    if let Some(f) = fault {
                        fault_hits.push(FaultHit {
                            fault_id: f.id,
                            endpoint: fault_endpoint.clone(),
                            actor_id: Some(event.actor_id.clone()),
                            effect_type: "latency".to_string(),
                        });
                    }
                    timeline_outcomes.push(TimelineOutcome {
                        at_ms: event.at_ms,
                        actor_id: event.actor_id,
                        action_type,
                        endpoint: fault_endpoint,
                        outcome: "error".to_string(),
                        error: Some(err_message.clone()),
                    });
                    status = "error".to_string();
                    error_message = Some(err_message.clone());
                    api_error = Some(map_error_string(&err_message));
                    break;
                }
            }
        }

        // --- assertions ---
        if status == "ok" {
            assertions = evaluate_assertions(state, &scenario).await;
            failed_assertions = assertions.iter().filter(|a| !a.passed).count();
            if failed_assertions > 0 {
                status = "assertion_failed".to_string();
                api_error = Some((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "scenario assertions failed"
                    })),
                ));
            }
        }
    }

    // --- finalize ---
    let (final_snapshot_revision, final_snapshot_hash, final_snapshot, final_events) = {
        let mut rt = state.runtime.lock().await;
        rt.kernel
            .set_metadata("scenario_name", scenario.name.clone());
        rt.kernel
            .set_metadata("scenario_faults", scenario.faults.len().to_string());
        rt.kernel
            .set_metadata("scenario_assertions", scenario.assertions.len().to_string());
        rt.kernel
            .set_metadata("scenario_assertion_failures", failed_assertions.to_string());
        let snapshot = full_snapshot(&rt);
        let hash = snapshot_hash(&snapshot);
        (snapshot.revision, hash, snapshot, rt.kernel.events().to_vec())
    };

    let report = ScenarioRunReport {
        run_id: run_id.clone(),
        source_run_id,
        scenario_name: scenario.name.clone(),
        status,
        seed: scenario.seed,
        start_time_unix_ms: scenario.start_time_unix_ms,
        events_applied,
        timeline_outcomes,
        fault_hits,
        assertions,
        failed_assertions,
        final_snapshot_revision,
        final_snapshot_hash,
        error: error_message,
    };

    {
        let mut scenario_runtime = state.scenario.lock().await;
        scenario_runtime.runs.insert(
            run_id,
            StoredRun {
                report: report.clone(),
                scenario: scenario.clone(),
            },
        );
    }

    let persist_result = {
        let artifact = RunArtifact {
            report: report.clone(),
            scenario,
            events: final_events,
            snapshot: final_snapshot,
        };
        let run_store = state.run_store.lock().await;
        run_store.persist_run(&artifact)
    };
    if let Err(message) = persist_result {
        api_error = Some((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": message })),
        ));
    }

    if let Some((status_code, _)) = &api_error {
        if *status_code == StatusCode::UNPROCESSABLE_ENTITY {
            api_error = Some((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!(report.clone())),
            ));
        }
    }

    (report, api_error)
}

/// Return the current wall-clock time in milliseconds since UNIX epoch.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Evaluate all assertions in a scenario by delegating to the twin.
pub async fn evaluate_assertions<T: TwinService>(
    state: &AppState<T>,
    scenario: &ScenarioDocument,
) -> Vec<AssertionResult> {
    let rt = state.runtime.lock().await;
    scenario
        .assertions
        .iter()
        .map(|rule| {
            let check_json = rule.check.clone();
            match rt.service.evaluate_assertion(&check_json) {
                Ok(mut result) => {
                    // Ensure the result uses the rule ID from the scenario.
                    result.id = rule.id.clone();
                    result
                }
                Err(err) => AssertionResult {
                    id: rule.id.clone(),
                    passed: false,
                    message: format!("assertion evaluation error: {err}"),
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Scenario validation
// ---------------------------------------------------------------------------

/// Validate a scenario document for structural correctness.
///
/// Checks generic structural properties: version, name, actor/fault/assertion
/// uniqueness, timeline actor references, fault probability ranges.  Delegates
/// domain-specific validation (initial_state contents, action payloads,
/// assertion check variants) to `T::validate_scenario()`.
pub fn validate_scenario<T: TwinService>(scenario: &ScenarioDocument) -> ScenarioValidationResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if scenario.version == 0 {
        errors.push("version must be >= 1".to_string());
    }
    if scenario.name.trim().is_empty() {
        errors.push("name must not be empty".to_string());
    }

    let mut actor_ids = BTreeSet::new();
    for actor in &scenario.actors {
        if actor.id.trim().is_empty() {
            errors.push("actor id must not be empty".to_string());
        } else if !actor_ids.insert(actor.id.clone()) {
            errors.push(format!("duplicate actor id `{}`", actor.id));
        }
    }

    // --- generic fault validation ---
    let mut fault_ids = BTreeSet::new();
    for fault in &scenario.faults {
        if fault.id.trim().is_empty() {
            errors.push("fault id must not be empty".to_string());
        } else if !fault_ids.insert(fault.id.clone()) {
            errors.push(format!("duplicate fault id `{}`", fault.id));
        }
        if !(0.0..=1.0).contains(&fault.when.probability) {
            errors.push(format!(
                "fault `{}` probability must be between 0.0 and 1.0",
                fault.id
            ));
        }
        if fault.when.endpoint.trim().is_empty() {
            errors.push(format!("fault `{}` endpoint must not be empty", fault.id));
        }
        if let Some(actor_id) = &fault.when.actor_id {
            if !actor_ids.contains(actor_id) {
                errors.push(format!(
                    "fault `{}` references unknown actor `{}`",
                    fault.id, actor_id
                ));
            }
        }
    }

    // --- generic assertion id validation ---
    let mut assertion_ids = BTreeSet::new();
    for assertion in &scenario.assertions {
        if assertion.id.trim().is_empty() {
            errors.push("assertion id must not be empty".to_string());
        } else if !assertion_ids.insert(assertion.id.clone()) {
            errors.push(format!("duplicate assertion id `{}`", assertion.id));
        }
    }

    // --- generic timeline validation ---
    let mut last_at_ms = 0u64;
    for (idx, event) in scenario.timeline.iter().enumerate() {
        if !actor_ids.contains(&event.actor_id) {
            errors.push(format!(
                "timeline event {} references unknown actor `{}`",
                idx, event.actor_id
            ));
        }
        if idx > 0 && event.at_ms < last_at_ms {
            warnings.push("timeline is not sorted by `at_ms`; runtime will sort it".to_string());
        }
        last_at_ms = event.at_ms;
    }

    // --- domain-specific validation via twin ---
    match serde_json::to_value(scenario) {
        Ok(scenario_json) => {
            let (twin_errors, twin_warnings) = T::validate_scenario(&scenario_json);
            errors.extend(twin_errors);
            warnings.extend(twin_warnings);
        }
        Err(e) => {
            errors.push(format!("failed to serialize scenario for domain validation: {e}"));
        }
    }

    ScenarioValidationResult {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Compute a deterministic hash string for a kernel snapshot.
pub fn snapshot_hash(snapshot: &twin_kernel::TwinState) -> String {
    let mut hasher = DefaultHasher::new();
    let payload = serde_json::to_vec(snapshot).unwrap_or_default();
    payload.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Capture a full snapshot that includes both kernel state and service state.
///
/// The kernel's `TwinState::service_state` field is populated with the
/// serialised domain state from `TwinService::service_snapshot()`.
fn full_snapshot<T: TwinService>(rt: &TwinRuntime<T>) -> twin_kernel::TwinState {
    let mut snap = rt.kernel.snapshot();
    snap.service_state = rt.service.service_snapshot();
    snap
}

/// Restore both kernel and service state from a snapshot.
///
/// The kernel is restored first, then the service is restored from the
/// `service_state` field. Service restore errors are propagated.
fn full_restore<T: TwinService>(
    rt: &mut TwinRuntime<T>,
    snapshot: twin_kernel::TwinState,
) -> Result<(), twin_service::TwinError> {
    let service_state = snapshot.service_state.clone();
    rt.kernel.restore(snapshot);
    rt.service.service_restore(&service_state)
}

/// Convert a `TwinError` message into an `ApiError`.
pub fn map_error(err: twin_service::TwinError) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
}

/// Build an `ApiError` from a plain error string.
fn map_error_string(msg: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

/// Returns `true` when the value contains only alphanumeric, `_`, or `-` chars.
pub fn is_safe_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Extract the action type string from a serialised action JSON.
///
/// Returns the value of the `"type"` field, or `"unknown"` if absent.
pub fn action_type_from_json(action: &serde_json::Value) -> &str {
    action["type"].as_str().unwrap_or("unknown")
}

/// Resolve a relative scenario path against a base directory, rejecting
/// absolute or traversal paths.
pub fn resolve_scenario_path(base_dir: &FsPath, relative: &str) -> Result<PathBuf, ApiError> {
    let candidate = FsPath::new(relative);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "scenario path must be relative within scenarios/" })),
        ));
    }
    Ok(base_dir.join(candidate))
}

/// Diff two run artifacts and produce a structured comparison.
pub fn diff_run_artifacts(a: &RunArtifact, b: &RunArtifact) -> RunDiffResult {
    let mut assertions_changed = Vec::new();
    let len = a.report.assertions.len().max(b.report.assertions.len());
    for i in 0..len {
        let lhs = a.report.assertions.get(i);
        let rhs = b.report.assertions.get(i);
        if lhs.map(|x| (&x.id, x.passed)) != rhs.map(|x| (&x.id, x.passed)) {
            let label = lhs
                .map(|x| x.id.clone())
                .or_else(|| rhs.map(|x| x.id.clone()))
                .unwrap_or_else(|| format!("index_{i}"));
            assertions_changed.push(label);
        }
    }

    let mut timeline_mismatch_indices = Vec::new();
    let tlen = a
        .report
        .timeline_outcomes
        .len()
        .max(b.report.timeline_outcomes.len());
    for i in 0..tlen {
        let lhs = a.report.timeline_outcomes.get(i);
        let rhs = b.report.timeline_outcomes.get(i);
        let same = lhs.map(|x| (&x.endpoint, &x.outcome, &x.error))
            == rhs.map(|x| (&x.endpoint, &x.outcome, &x.error));
        if !same {
            timeline_mismatch_indices.push(i);
        }
    }

    RunDiffResult {
        run_id_a: a.report.run_id.clone(),
        run_id_b: b.report.run_id.clone(),
        status_a: a.report.status.clone(),
        status_b: b.report.status.clone(),
        status_equal: a.report.status == b.report.status,
        snapshot_hash_a: a.report.final_snapshot_hash.clone(),
        snapshot_hash_b: b.report.final_snapshot_hash.clone(),
        snapshot_hash_equal: a.report.final_snapshot_hash == b.report.final_snapshot_hash,
        events_count_a: a.events.len(),
        events_count_b: b.events.len(),
        events_count_equal: a.events.len() == b.events.len(),
        assertions_changed,
        timeline_mismatch_indices,
        fault_hit_count_a: a.report.fault_hits.len(),
        fault_hit_count_b: b.report.fault_hits.len(),
        fault_hit_count_equal: a.report.fault_hits.len() == b.report.fault_hits.len(),
    }
}

/// Check whether a diff indicates deterministic replay.
pub fn is_deterministic_match(diff: &RunDiffResult) -> bool {
    diff.status_equal
        && diff.snapshot_hash_equal
        && diff.events_count_equal
        && diff.assertions_changed.is_empty()
        && diff.timeline_mismatch_indices.is_empty()
        && diff.fault_hit_count_equal
}

/// Load a run artifact from the in-memory store or from disk.
pub async fn load_run_artifact<T: TwinService>(
    state: &AppState<T>,
    run_id: &str,
) -> Result<RunArtifact, ApiError> {
    if !is_safe_token(run_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid run id" })),
        ));
    }
    let artifact = {
        let run_store = state.run_store.lock().await;
        run_store.load_run(run_id)
    };
    artifact.map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "scenario run not found" })),
        )
    })
}

// ---------------------------------------------------------------------------
// Control surface routes (generic over T)
// ---------------------------------------------------------------------------

/// Build the control-surface router.
///
/// Returns an Axum `Router` with all `/health` and `/control/*` routes
/// wired up.  The caller can merge this with the twin's own domain
/// routes (from `T::routes()`).
pub fn control_routes<T: TwinService>(state: AppState<T>) -> Router {
    Router::new()
        .route("/health", get(health::<T>))
        .route("/control/reset", post(control_reset::<T>))
        .route("/control/seed", post(control_seed::<T>))
        .route("/control/snapshot", get(control_snapshot::<T>))
        .route("/control/restore", post(control_restore::<T>))
        .route("/control/events", get(control_events::<T>))
        .route(
            "/control/scenario/validate",
            post(control_validate_scenario::<T>),
        )
        .route(
            "/control/scenario/runs",
            get(control_list_scenario_runs::<T>),
        )
        .route(
            "/control/scenario/runs/{run_id}",
            get(control_get_scenario_run::<T>),
        )
        .route(
            "/control/scenario/runs/{run_id}/bundle",
            get(control_get_scenario_run_bundle::<T>),
        )
        .route(
            "/control/scenario/runs/{run_id}/verify-replay",
            post(control_verify_replay::<T>),
        )
        .route(
            "/control/scenario/runs/diff",
            post(control_diff_scenario_runs::<T>),
        )
        .route(
            "/control/scenario/apply",
            post(control_apply_scenario::<T>),
        )
        .route(
            "/control/scenario/apply-file",
            post(control_apply_scenario_file::<T>),
        )
        .route(
            "/control/scenario/replay",
            post(control_replay_scenario::<T>),
        )
        // Session management
        .route(
            "/control/sessions",
            get(control_list_sessions::<T>).post(control_create_session::<T>),
        )
        .route(
            "/control/sessions/{id}",
            get(control_get_session::<T>),
        )
        .route(
            "/control/sessions/{id}/end",
            post(control_end_session::<T>),
        )
        .route(
            "/control/sessions/{id}/events",
            get(control_session_events::<T>),
        )
        .route(
            "/control/sessions/{id}/snapshot",
            get(control_session_snapshot::<T>),
        )
        .with_state(state)
}

// --- handlers ---

async fn health<T: TwinService>(
    State(state): State<AppState<T>>,
) -> Json<serde_json::Value> {
    let rt = state.runtime.lock().await;
    let snapshot = rt.kernel.snapshot();
    Json(serde_json::json!({
        "status": "ok",
        "revision": snapshot.revision
    }))
}

async fn control_reset<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(config): Json<TwinConfig>,
) -> Json<serde_json::Value> {
    let mut rt = state.runtime.lock().await;
    rt.kernel.clear_active_session();
    rt.reset(config);
    drop(rt);

    let mut scenario = state.scenario.lock().await;
    scenario.faults.clear();
    scenario.decision_index = 0;

    Json(serde_json::json!({ "status": "ok" }))
}

/// Seed the twin with initial state.
///
/// Resets the twin (kernel + service) and then calls `seed_from_scenario`
/// with the request body as the `initial_state`.  This is a simplified
/// alternative to `POST /control/scenario/apply` — the caller provides
/// the domain-specific seed data directly without wrapping it in a
/// full scenario document.
///
/// **Drive example:** `{"files": [{"id": "f1", "name": "doc.txt", ...}]}`
/// **Gmail example:** `{"messages": [...], "labels": [...]}`
async fn control_seed<T: TwinService>(
    State(state): State<AppState<T>>,
    body: Bytes,
) -> ApiResult<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(format_seed_json_error(e)),
        )
    })?;

    if !body.is_object() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(seed_error_json(
                "invalid_seed_payload",
                "seed payload must be a JSON object",
                vec![serde_json::json!({
                    "path": "$",
                    "message": format!("expected object, got {}", body_type_name(&body)),
                })],
            )),
        ));
    }

    let mut rt = state.runtime.lock().await;
    rt.kernel.clear_active_session();
    rt.reset(TwinConfig {
        seed: 0,
        start_time_unix_ms: 0,
    });

    rt.service.seed_from_scenario(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(format_seed_service_error(&e.to_string())),
        )
    })?;

    // Clear fault rules so the twin starts clean.
    drop(rt);
    let mut scenario = state.scenario.lock().await;
    scenario.faults.clear();
    scenario.decision_index = 0;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

fn body_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

async fn control_snapshot<T: TwinService>(
    State(state): State<AppState<T>>,
) -> Json<twin_kernel::TwinState> {
    let rt = state.runtime.lock().await;
    Json(full_snapshot(&rt))
}

async fn control_restore<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(snapshot): Json<twin_kernel::TwinState>,
) -> ApiResult<serde_json::Value> {
    let mut rt = state.runtime.lock().await;
    full_restore(&mut rt, snapshot).map_err(map_error)?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

async fn control_events<T: TwinService>(
    State(state): State<AppState<T>>,
    Query(params): Query<EventQueryParams>,
) -> Json<Vec<TwinEvent>> {
    let rt = state.runtime.lock().await;
    let filtered = rt
        .kernel
        .events()
        .iter()
        .filter(|e| {
            if let Some(ref actor_id) = params.actor_id {
                if e.actor_id.as_deref() != Some(actor_id.as_str()) {
                    return false;
                }
            }
            if let Some(ref endpoint) = params.endpoint {
                if e.endpoint != *endpoint {
                    return false;
                }
            }
            if let Some(ref action) = params.action {
                if e.detail != *action {
                    return false;
                }
            }
            if let Some(ref outcome) = params.outcome {
                if e.outcome != *outcome {
                    return false;
                }
            }
            if let Some(after) = params.after {
                if e.logical_time_unix_ms <= after {
                    return false;
                }
            }
            if let Some(before) = params.before {
                if e.logical_time_unix_ms >= before {
                    return false;
                }
            }
            if let Some(ref session_id) = params.session_id {
                if e.session_id.as_deref() != Some(session_id.as_str()) {
                    return false;
                }
            }
            true
        })
        .cloned();
    let events: Vec<TwinEvent> = if let Some(limit) = params.limit {
        filtered.take(limit).collect()
    } else {
        filtered.collect()
    };
    Json(events)
}

async fn control_validate_scenario<T: TwinService>(
    Json(scenario): Json<ScenarioDocument>,
) -> ApiResult<serde_json::Value> {
    let validation = validate_scenario::<T>(&scenario);
    if !validation.valid {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "scenario validation failed",
                "validation": validation
            })),
        ));
    }
    Ok(Json(serde_json::json!({
        "status": "ok",
        "validation": validation
    })))
}

async fn control_apply_scenario<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(scenario): Json<ScenarioDocument>,
) -> ApiResult<serde_json::Value> {
    let validation = validate_scenario::<T>(&scenario);
    if !validation.valid {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "scenario validation failed",
                "validation": validation
            })),
        ));
    }

    let (report, api_error) = run_scenario(&state, scenario, None).await;
    if let Some(err) = api_error {
        return Err(err);
    }
    Ok(Json(serde_json::json!(report)))
}

async fn control_apply_scenario_file<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(body): Json<ApplyFileRequest>,
) -> ApiResult<serde_json::Value> {
    let path = resolve_scenario_path(&state.scenarios_dir, &body.path)?;
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("failed to read scenario file: {e}") })),
        )
    })?;
    let scenario = parse_scenario_json(&raw).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid scenario json: {e}") })),
        )
    })?;
    let validation = validate_scenario::<T>(&scenario);
    if !validation.valid {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "scenario validation failed",
                "validation": validation
            })),
        ));
    }

    let (report, api_error) = run_scenario(&state, scenario, None).await;
    if let Some(err) = api_error {
        return Err(err);
    }
    Ok(Json(serde_json::json!(report)))
}

async fn control_get_scenario_run<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(run_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    {
        let scenario_runtime = state.scenario.lock().await;
        if let Some(stored) = scenario_runtime.runs.get(&run_id) {
            return Ok(Json(serde_json::json!(stored.report.clone())));
        }
    }

    let artifact = {
        let run_store = state.run_store.lock().await;
        run_store.load_run(&run_id)
    };
    match artifact {
        Ok(artifact) => Ok(Json(serde_json::json!(artifact.report))),
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "scenario run not found" })),
        )),
    }
}

async fn control_get_scenario_run_bundle<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(run_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    if !is_safe_token(&run_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid run id" })),
        ));
    }

    if let Some(stored) = {
        let scenario_runtime = state.scenario.lock().await;
        scenario_runtime.runs.get(&run_id).cloned()
    } {
        let (events, snapshot) = {
            let rt = state.runtime.lock().await;
            (rt.kernel.events().to_vec(), full_snapshot(&rt))
        };
        let artifact = RunArtifact {
            report: stored.report,
            scenario: stored.scenario,
            events,
            snapshot,
        };
        return Ok(Json(serde_json::json!(artifact)));
    }

    let artifact = {
        let run_store = state.run_store.lock().await;
        run_store.load_run(&run_id)
    };
    match artifact {
        Ok(artifact) => Ok(Json(serde_json::json!(artifact))),
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "scenario run not found" })),
        )),
    }
}

async fn control_list_scenario_runs<T: TwinService>(
    State(state): State<AppState<T>>,
    Query(query): Query<RunListQuery>,
) -> ApiResult<serde_json::Value> {
    let mut by_id: BTreeMap<String, RunListItem> = BTreeMap::new();
    {
        let run_store = state.run_store.lock().await;
        for report in run_store.list_run_reports() {
            by_id.insert(
                report.run_id.clone(),
                RunListItem {
                    run_id: report.run_id,
                    scenario_name: report.scenario_name,
                    status: report.status,
                    seed: report.seed,
                    start_time_unix_ms: report.start_time_unix_ms,
                    final_snapshot_hash: report.final_snapshot_hash,
                },
            );
        }
    }

    {
        let scenario_runtime = state.scenario.lock().await;
        for stored in scenario_runtime.runs.values() {
            by_id.insert(
                stored.report.run_id.clone(),
                RunListItem {
                    run_id: stored.report.run_id.clone(),
                    scenario_name: stored.report.scenario_name.clone(),
                    status: stored.report.status.clone(),
                    seed: stored.report.seed,
                    start_time_unix_ms: stored.report.start_time_unix_ms,
                    final_snapshot_hash: stored.report.final_snapshot_hash.clone(),
                },
            );
        }
    }
    let mut items: Vec<RunListItem> = by_id.into_values().collect();

    if let Some(status) = query.status.as_deref() {
        items.retain(|it| it.status == status);
    }
    if let Some(name) = query.scenario_name.as_deref() {
        items.retain(|it| it.scenario_name == name);
    }
    items.sort_by(|a, b| b.run_id.cmp(&a.run_id));
    if let Some(limit) = query.limit {
        items.truncate(limit);
    }

    Ok(Json(serde_json::json!({ "runs": items })))
}

async fn control_diff_scenario_runs<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(body): Json<RunDiffRequest>,
) -> ApiResult<serde_json::Value> {
    let artifact_a = load_run_artifact(&state, &body.run_id_a).await?;
    let artifact_b = load_run_artifact(&state, &body.run_id_b).await?;
    let diff = diff_run_artifacts(&artifact_a, &artifact_b);
    Ok(Json(serde_json::json!(diff)))
}

async fn control_verify_replay<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(run_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let baseline = load_run_artifact(&state, &run_id).await?;
    let (replay_report, replay_error) =
        run_scenario(&state, baseline.scenario.clone(), Some(run_id.clone())).await;
    if let Some(err) = replay_error {
        return Err(err);
    }
    let replay = load_run_artifact(&state, &replay_report.run_id).await?;
    let diff = diff_run_artifacts(&baseline, &replay);
    let ok = is_deterministic_match(&diff);

    Ok(Json(serde_json::json!({
        "ok": ok,
        "baseline_run_id": baseline.report.run_id,
        "replay_run_id": replay.report.run_id,
        "diff": diff
    })))
}

async fn control_replay_scenario<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(body): Json<ReplayRequest>,
) -> ApiResult<serde_json::Value> {
    let scenario = {
        let scenario_runtime = state.scenario.lock().await;
        scenario_runtime
            .runs
            .get(&body.run_id)
            .map(|s| s.scenario.clone())
    };
    let scenario = if let Some(s) = scenario {
        s
    } else {
        let artifact = {
            let run_store = state.run_store.lock().await;
            run_store.load_run(&body.run_id)
        };
        match artifact {
            Ok(artifact) => artifact.scenario,
            Err(_) => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "scenario run not found" })),
                ))
            }
        }
    };

    let (report, api_error) = run_scenario(&state, scenario, Some(body.run_id)).await;
    if let Some(err) = api_error {
        return Err(err);
    }
    Ok(Json(serde_json::json!(report)))
}

// --- session handlers ---

async fn control_list_sessions<T: TwinService>(
    State(state): State<AppState<T>>,
) -> Json<serde_json::Value> {
    let session_store = state.session_store.lock().await;
    let sessions: Vec<serde_json::Value> = session_store
        .list()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "session_id": s.session_id,
                "name": s.name,
                "status": s.status,
                "created_at_unix_ms": s.created_at_unix_ms,
                "ended_at_unix_ms": s.ended_at_unix_ms,
                "final_event_count": s.final_event_count,
            })
        })
        .collect();
    Json(serde_json::json!({ "sessions": sessions }))
}

/// Helper: end a session in-place, capturing frozen snapshot and events.
/// Caller must hold both locks.
fn end_session_in_place<T: TwinService>(
    session: &mut SessionInfo,
    rt: &TwinRuntime<T>,
) {
    let session_id = session.session_id.clone();
    let frozen: Vec<TwinEvent> = rt
        .kernel
        .events()
        .iter()
        .filter(|e| e.session_id.as_deref() == Some(&session_id))
        .cloned()
        .collect();
    session.status = SessionStatus::Ended;
    session.ended_at_unix_ms = Some(now_unix_ms());
    session.final_event_count = frozen.len();
    session.final_snapshot = Some(full_snapshot(rt));
    session.frozen_events = Some(frozen);
}

async fn control_create_session<T: TwinService>(
    State(state): State<AppState<T>>,
    Json(body): Json<CreateSessionRequest>,
) -> ApiResult<serde_json::Value> {
    // Auto-end any currently active session before creating a new one.
    // First, check if there's an active session (clone the id to drop the lock).
    let active_id = {
        let session_store = state.session_store.lock().await;
        session_store.active_session().map(|s| s.session_id.clone())
    };

    if let Some(old_id) = active_id {
        // Capture frozen data from runtime, then update session store.
        let mut rt = state.runtime.lock().await;
        let mut session_store = state.session_store.lock().await;
        if let Some(old_session) = session_store.get_mut(&old_id) {
            if old_session.status == SessionStatus::Active {
                end_session_in_place::<T>(old_session, &rt);
            }
        }
        rt.kernel.clear_active_session();
        drop(session_store);
        drop(rt);
    }

    // Reset twin state
    {
        let mut rt = state.runtime.lock().await;
        let config = rt.kernel.config().clone();
        rt.reset(config);
    }

    // Apply seed data if provided
    if let Some(seed_data) = &body.seed {
        let mut rt = state.runtime.lock().await;
        rt.service
            .seed_from_scenario(seed_data)
            .map_err(map_error)?;
    }

    // Create session
    let session_id = {
        let mut session_store = state.session_store.lock().await;
        session_store.create(body.name, now_unix_ms())
    };

    // Set active session in kernel
    {
        let mut rt = state.runtime.lock().await;
        rt.kernel.set_active_session(session_id.clone());
    }

    Ok(Json(serde_json::json!({ "session_id": session_id })))
}

async fn control_get_session<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    // Clone needed data from session_store, then drop the lock before
    // potentially acquiring the runtime lock (deadlock prevention).
    let session = {
        let session_store = state.session_store.lock().await;
        session_store.get(&id).cloned().ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
        })?
    };

    // For active sessions, compute live event count from the kernel.
    // For ended sessions, use the frozen final_event_count.
    let event_count = match session.status {
        SessionStatus::Active => {
            let rt = state.runtime.lock().await;
            rt.kernel
                .events()
                .iter()
                .filter(|e| e.session_id.as_deref() == Some(&id))
                .count()
        }
        SessionStatus::Ended => session.final_event_count,
    };

    Ok(Json(serde_json::json!({
        "session_id": session.session_id,
        "name": session.name,
        "status": session.status,
        "created_at_unix_ms": session.created_at_unix_ms,
        "ended_at_unix_ms": session.ended_at_unix_ms,
        "event_count": event_count,
    })))
}

async fn control_end_session<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    // Clone session status to check validity, drop the lock before
    // acquiring runtime (deadlock prevention).
    let current_status = {
        let session_store = state.session_store.lock().await;
        let session = session_store.get(&id).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
        })?;
        session.status.clone()
    };

    if current_status == SessionStatus::Ended {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "session already ended" })),
        ));
    }

    // Capture frozen snapshot and events from the runtime.
    let rt = state.runtime.lock().await;
    let snapshot = full_snapshot(&rt);
    let frozen_events: Vec<TwinEvent> = rt
        .kernel
        .events()
        .iter()
        .filter(|e| e.session_id.as_deref() == Some(&id))
        .cloned()
        .collect();
    let event_count = frozen_events.len();
    drop(rt);

    // Update session info
    {
        let mut session_store = state.session_store.lock().await;
        let session = session_store.get_mut(&id).unwrap();
        session.status = SessionStatus::Ended;
        session.ended_at_unix_ms = Some(now_unix_ms());
        session.final_event_count = event_count;
        session.final_snapshot = Some(snapshot);
        session.frozen_events = Some(frozen_events);
    }

    // Clear active session in kernel
    {
        let mut rt = state.runtime.lock().await;
        if rt.kernel.active_session() == Some(&id) {
            rt.kernel.clear_active_session();
        }
    }

    Ok(Json(serde_json::json!({
        "status": "ok",
        "session_id": id,
        "event_count": event_count,
    })))
}

async fn control_session_events<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(id): Path<String>,
) -> ApiResult<Vec<TwinEvent>> {
    // Clone session data and drop session_store lock before acquiring runtime
    // (deadlock prevention). For ended sessions, return frozen_events directly.
    let session = {
        let session_store = state.session_store.lock().await;
        session_store.get(&id).cloned().ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
        })?
    };

    match session.status {
        SessionStatus::Ended => {
            // Return frozen events captured at session end.
            let events = session.frozen_events.unwrap_or_default();
            Ok(Json(events))
        }
        SessionStatus::Active => {
            let rt = state.runtime.lock().await;
            let events: Vec<TwinEvent> = rt
                .kernel
                .events()
                .iter()
                .filter(|e| e.session_id.as_deref() == Some(&id))
                .cloned()
                .collect();
            Ok(Json(events))
        }
    }
}

async fn control_session_snapshot<T: TwinService>(
    State(state): State<AppState<T>>,
    Path(id): Path<String>,
) -> ApiResult<twin_kernel::TwinState> {
    // Clone needed data from session_store, drop lock before acquiring runtime
    // (deadlock prevention).
    let session = {
        let session_store = state.session_store.lock().await;
        session_store.get(&id).cloned().ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
        })?
    };

    match &session.status {
        SessionStatus::Ended => {
            // Return the frozen snapshot captured at session end
            let snapshot = session.final_snapshot.ok_or_else(|| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "no snapshot available for ended session" })),
                )
            })?;
            Ok(Json(snapshot))
        }
        SessionStatus::Active => {
            // Return current live snapshot
            let rt = state.runtime.lock().await;
            Ok(Json(full_snapshot(&rt)))
        }
    }
}

// ---------------------------------------------------------------------------
// Auth configuration
// ---------------------------------------------------------------------------

/// Token-to-actor mapping configuration for request authentication.
///
/// Loaded from a JSON file at the path given by `TWIN_AUTH_FILE` (default
/// `./actors.json`).  The file should be a JSON object mapping bearer tokens
/// to actor IDs, for example:
///
/// ```json
/// { "tok_alice": "alice", "tok_bob": "bob" }
/// ```
///
/// Resolution priority (highest to lowest):
///
/// 1. `X-Twin-Actor-Id` header — used directly (testing convenience).
/// 2. `Authorization: Bearer <token>` — looked up in the actors map; if the
///    token is not found, a deterministic hash-based fallback produces
///    `actor_<16 hex chars of u64 hash>`.
/// 3. No auth headers — falls back to `"default"`.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Token → actor_id mapping.
    pub actors: HashMap<String, String>,
    /// If true, requests without valid auth headers are rejected with 401
    /// instead of falling back to the `"default"` actor.
    pub reject_unauthenticated: bool,
}

impl AuthConfig {
    /// Load auth config from a JSON file.  Returns an empty config if the
    /// file does not exist.
    pub fn from_file(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                match serde_json::from_str::<HashMap<String, String>>(&contents) {
                    Ok(actors) => {
                        tracing::info!(
                            path = %path.display(),
                            actor_count = actors.len(),
                            "loaded auth config"
                        );
                        AuthConfig {
                            actors,
                            reject_unauthenticated: false,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "auth config file contains invalid JSON, using empty actor map"
                        );
                        AuthConfig {
                            actors: HashMap::new(),
                            reject_unauthenticated: false,
                        }
                    }
                }
            }
            Err(_) => {
                tracing::info!(
                    path = %path.display(),
                    "auth config file not found, using empty actor map"
                );
                AuthConfig {
                    actors: HashMap::new(),
                    reject_unauthenticated: false,
                }
            }
        }
    }

    /// Resolve an actor ID from request headers.
    ///
    /// See [`AuthConfig`] doc for priority rules.
    ///
    /// Returns `None` when [`reject_unauthenticated`](AuthConfig::reject_unauthenticated)
    /// is `true` and no authentication headers are present.
    pub fn resolve_actor(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        // Priority 1: explicit X-Twin-Actor-Id header
        if let Some(val) = headers.get("X-Twin-Actor-Id") {
            if let Ok(s) = val.to_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }

        // Priority 2: Authorization: Bearer <token> (case-insensitive prefix per RFC 7235)
        if let Some(auth_val) = headers.get("Authorization") {
            if let Ok(auth_str) = auth_val.to_str() {
                let token = if auth_str.len() > 7
                    && auth_str[..7].eq_ignore_ascii_case("Bearer ")
                {
                    Some(auth_str[7..].trim())
                } else {
                    None
                };
                if let Some(token) = token {
                    if !token.is_empty() {
                        // Look up in actors map
                        if let Some(actor_id) = self.actors.get(token) {
                            return Some(actor_id.clone());
                        }
                        // Hash-based fallback (full u64 for collision resistance)
                        let mut hasher = DefaultHasher::new();
                        token.hash(&mut hasher);
                        let hash = hasher.finish();
                        return Some(format!("actor_{:016x}", hash));
                    }
                }
            }
        }

        // Priority 3: no auth headers
        if self.reject_unauthenticated {
            None
        } else {
            Some("default".to_string())
        }
    }
}

/// Axum middleware that resolves the actor identity from request headers and
/// injects a [`ResolvedActorId`] extension for downstream handlers.
///
/// Returns 401 Unauthorized if [`AuthConfig::reject_unauthenticated`] is set
/// and no authentication headers are present.
async fn auth_middleware(
    Extension(auth): Extension<AuthConfig>,
    request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    match auth.resolve_actor(request.headers()) {
        Some(actor_id) => {
            let mut request = request;
            request.extensions_mut().insert(ResolvedActorId(actor_id));
            next.run(request).await
        }
        None => {
            let body = serde_json::json!({
                "error": {
                    "code": 401,
                    "message": "Authentication required. Provide an Authorization header or X-Twin-Actor-Id header."
                }
            });
            (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(body),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Builder helper
// ---------------------------------------------------------------------------

/// Configuration for building a twin server.
#[derive(Debug)]
pub struct ServerConfig {
    pub runs_dir: PathBuf,
    pub scenarios_dir: PathBuf,
    pub twin_config: TwinConfig,
    pub auth: AuthConfig,
}

/// Full environment-derived configuration for a twin server binary.
///
/// Reads from environment variables with sensible defaults:
///
/// | Variable             | Default           | Description                    |
/// |----------------------|-------------------|--------------------------------|
/// | `TWIN_HOST`          | `127.0.0.1`       | HTTP listen address            |
/// | `TWIN_PORT`          | `8080`            | HTTP listen port               |
/// | `TWIN_SCENARIOS_DIR` | `./scenarios`     | Scenario file directory        |
/// | `TWIN_RUNS_DIR`      | `./runs`          | Run artifact persistence dir   |
/// | `TWIN_LOG_LEVEL`     | `info`            | tracing log level filter       |
/// | `TWIN_AUTH_FILE`     | `./actors.json`   | Token-to-actor mapping file    |
/// | `TWIN_REJECT_UNAUTH` | `false`           | Reject unauthenticated requests with 401 |
///
/// `TWIN_LOG_LEVEL` takes precedence over `RUST_LOG`. If neither is set,
/// defaults to `info`.
#[derive(Debug)]
pub struct EnvConfig {
    pub host: [u8; 4],
    pub port: u16,
    pub log_level: String,
    pub server: ServerConfig,
}

impl EnvConfig {
    /// Read configuration from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        Self::from_env_with(|key| std::env::var(key))
    }

    /// Internal constructor that accepts a lookup function (for testing).
    fn from_env_with<F>(get_var: F) -> Self
    where
        F: Fn(&str) -> Result<String, std::env::VarError>,
    {
        let port = get_var("TWIN_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8080);

        let host: [u8; 4] = get_var("TWIN_HOST")
            .ok()
            .filter(|v| !v.is_empty())
            .and_then(|v| {
                let parts: Vec<u8> = v.split('.').filter_map(|p| p.parse().ok()).collect();
                if parts.len() == 4 {
                    Some([parts[0], parts[1], parts[2], parts[3]])
                } else {
                    None
                }
            })
            .unwrap_or([127, 0, 0, 1]);

        let cwd = std::env::current_dir().expect("resolve cwd");

        let scenarios_dir = get_var("TWIN_SCENARIOS_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.join("scenarios"));

        let runs_dir = get_var("TWIN_RUNS_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.join("runs"));

        // TWIN_LOG_LEVEL takes precedence, then RUST_LOG, then "info".
        let log_level = get_var("TWIN_LOG_LEVEL")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| get_var("RUST_LOG").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "info".to_string());

        let auth_file = get_var("TWIN_AUTH_FILE")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.join("actors.json"));

        let reject_unauthenticated = get_var("TWIN_REJECT_UNAUTH")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let mut auth = AuthConfig::from_file(&auth_file);
        auth.reject_unauthenticated = reject_unauthenticated;

        EnvConfig {
            host,
            port,
            log_level,
            server: ServerConfig {
                runs_dir,
                scenarios_dir,
                twin_config: TwinConfig {
                    seed: 42,
                    start_time_unix_ms: 1_704_067_200_000,
                },
                auth,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery document support
// ---------------------------------------------------------------------------

/// Build a Google API discovery document from [`DiscoveryMeta`] and request context.
///
/// The `root_url` is determined from the request's Host header at runtime so
/// that SDKs can build correct URLs regardless of the twin's actual address.
fn build_discovery_document(meta: &DiscoveryMeta, root_url: &str) -> serde_json::Value {
    fn resource_to_json(res: &DiscoveryResource) -> serde_json::Value {
        let mut obj = serde_json::Map::new();

        if !res.methods.is_empty() {
            let methods: serde_json::Map<String, serde_json::Value> = res
                .methods
                .iter()
                .map(|(name, m)| (name.clone(), method_to_json(m)))
                .collect();
            obj.insert("methods".to_string(), serde_json::Value::Object(methods));
        }

        if !res.resources.is_empty() {
            let nested: serde_json::Map<String, serde_json::Value> = res
                .resources
                .iter()
                .map(|(name, r)| (name.clone(), resource_to_json(r)))
                .collect();
            obj.insert(
                "resources".to_string(),
                serde_json::Value::Object(nested),
            );
        }

        serde_json::Value::Object(obj)
    }

    fn method_to_json(m: &DiscoveryMethod) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "id": m.id,
            "httpMethod": m.http_method,
            "path": m.path,
        });
        if !m.description.is_empty() {
            obj["description"] = serde_json::Value::String(m.description.clone());
        }
        if !m.parameters.is_empty() {
            obj["parameters"] = serde_json::Value::Object(
                m.parameters
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            );
        }
        if !m.parameter_order.is_empty() {
            obj["parameterOrder"] =
                serde_json::Value::Array(m.parameter_order.iter().map(|s| serde_json::json!(s)).collect());
        }
        if m.supports_media_upload {
            obj["supportsMediaUpload"] = serde_json::Value::Bool(true);
            if let Some(ref mu) = m.media_upload {
                obj["mediaUpload"] = mu.clone();
            }
        }
        if let Some(ref req) = m.request {
            obj["request"] = req.clone();
        }
        if let Some(ref resp) = m.response {
            obj["response"] = resp.clone();
        }
        obj
    }

    let resources: serde_json::Map<String, serde_json::Value> = meta
        .resources
        .iter()
        .map(|(name, r)| (name.clone(), resource_to_json(r)))
        .collect();

    serde_json::json!({
        "kind": "discovery#restDescription",
        "discoveryVersion": "v1",
        "id": format!("{}:{}", meta.name, meta.version),
        "name": meta.name,
        "version": meta.version,
        "title": meta.title,
        "description": meta.description,
        "protocol": "rest",
        "rootUrl": root_url,
        "servicePath": meta.service_path,
        "baseUrl": format!("{}{}", root_url, meta.service_path),
        "batchPath": "batch",
        "resources": resources,
        "schemas": meta.schemas,
        "auth": {
            "oauth2": {
                "scopes": {}
            }
        }
    })
}

/// Extract root URL from the request Host header.
///
/// Falls back to `http://localhost` if no Host header is present.
fn root_url_from_request(headers: &axum::http::HeaderMap) -> String {
    if let Some(host) = headers.get("host") {
        if let Ok(h) = host.to_str() {
            return format!("http://{}/", h);
        }
    }
    "http://localhost/".to_string()
}

/// Query parameters for the V2-style discovery endpoint.
#[derive(Deserialize)]
struct DiscoveryQuery {
    #[allow(dead_code)]
    version: Option<String>,
}

/// V2-style discovery endpoint: `GET /$discovery/rest?version={version}`
async fn discovery_v2_handler<T: TwinService>(
    Query(_query): Query<DiscoveryQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    match T::discovery_meta() {
        Some(meta) => {
            let root_url = root_url_from_request(request.headers());
            let doc = build_discovery_document(&meta, &root_url);
            (StatusCode::OK, Json(doc)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "this twin does not serve a discovery document"
            })),
        )
            .into_response(),
    }
}

/// V1-style discovery endpoint: `GET /discovery/v1/apis/{api}/{version}/rest`
async fn discovery_v1_handler<T: TwinService>(
    Path((_api, _version)): Path<(String, String)>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    match T::discovery_meta() {
        Some(meta) => {
            let root_url = root_url_from_request(request.headers());
            let doc = build_discovery_document(&meta, &root_url);
            (StatusCode::OK, Json(doc)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "this twin does not serve a discovery document"
            })),
        )
            .into_response(),
    }
}

/// Build discovery routes for a twin.
///
/// These routes are intentionally placed **outside** the auth middleware layer
/// so they can be accessed without authentication — SDKs fetch the discovery
/// document before making authenticated API calls.
fn discovery_routes<T: TwinService>() -> Router {
    Router::new()
        .route("/$discovery/rest", get(discovery_v2_handler::<T>))
        .route(
            "/discovery/v1/apis/{api}/{version}/rest",
            get(discovery_v1_handler::<T>),
        )
}

// ---------------------------------------------------------------------------
// Event recording middleware
// ---------------------------------------------------------------------------

/// Middleware that records a [`TwinEvent`] for every data-plane request.
///
/// This is applied only to the twin routes (NOT control routes), so that
/// consumers can observe all API interactions via `GET /control/events`.
///
/// The middleware captures the HTTP method and URI path from the inbound
/// request, the resolved actor identity (if present), and the response
/// status code.  After the inner handler returns, it locks the runtime
/// and calls [`TwinKernel::record_event`].
async fn event_recording_middleware<T: TwinService>(
    Extension(runtime): Extension<SharedTwinState<T>>,
    request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let request_id = request
        .headers()
        .get("X-Request-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generate_request_id);
    let trace_id = extract_trace_id(request.headers());

    // Skip recording for state-inspection routes — these are read-only
    // introspection endpoints, not data-plane operations.
    if path.starts_with("/state/") {
        let mut response = next.run(request).await;
        if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
            response.headers_mut().insert("X-Twin-Request-Id", value);
        }
        return response;
    }

    let actor_id = request
        .extensions()
        .get::<ResolvedActorId>()
        .map(|r| r.0.clone());

    let mut response = next.run(request).await;

    let status = response.status();
    let outcome = if status.is_success() { "ok" } else { "error" };
    let detail = format!("{} {}", method, path);

    {
        let mut rt: tokio::sync::MutexGuard<'_, TwinRuntime<T>> = runtime.lock().await;
        rt.kernel.record_event_with_context(
            &path,
            actor_id,
            outcome,
            &detail,
            None,
            TwinEventContext {
                operation: Some(method.to_string()),
                resource: Some(path.clone()),
                request_id: Some(request_id.clone()),
                trace_id,
            },
        );
    }

    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("X-Twin-Request-Id", value);
    }

    response
}

/// Build a complete twin-server `Router`:  control surface + twin routes.
///
/// This is the primary entry point for creating a twin server.
///
/// The router includes an auth middleware layer that resolves actor identity
/// from request headers (see [`AuthConfig`]) and injects a
/// [`ResolvedActorId`] extension for downstream handlers.
pub fn build_twin_router<T: TwinService>(config: ServerConfig) -> Router {
    let run_store = RunStore::new(config.runs_dir).expect("initialize run store");
    let next_run_id = run_store.next_run_id();

    let runtime = Arc::new(Mutex::new(TwinRuntime::new(
        twin_kernel::TwinKernel::new(config.twin_config),
        T::default(),
    )));

    let state = AppState::<T> {
        runtime: runtime.clone(),
        scenario: Arc::new(Mutex::new(ScenarioRuntime::new(next_run_id))),
        run_store: Arc::new(Mutex::new(run_store)),
        session_store: Arc::new(Mutex::new(SessionStore::new())),
        scenarios_dir: config.scenarios_dir,
    };

    // Wrap twin routes with event recording middleware so that every
    // data-plane request is captured in the kernel event log.
    let twin_routes = T::routes(runtime.clone())
        .layer(middleware::from_fn(event_recording_middleware::<T>))
        .layer(Extension(runtime));

    let control = control_routes(state);
    let authed = control
        .merge(twin_routes)
        .layer(middleware::from_fn(auth_middleware))
        .layer(Extension(config.auth));

    // Discovery routes sit outside the auth layer so SDKs can fetch them
    // without credentials.
    authed.merge(discovery_routes::<T>())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::util::ServiceExt;
    use twin_service::{TimelineActionResult, TwinError};

    // -- Stub twin for testing --

    #[derive(Default, Serialize, Deserialize)]
    struct StubTwin {
        items: Vec<String>,
    }

    impl TwinService for StubTwin {
        fn routes(_shared: twin_service::SharedTwinState<Self>) -> Router {
            Router::new()
        }

        fn service_snapshot(&self) -> serde_json::Value {
            serde_json::json!({ "items": self.items })
        }

        fn service_restore(&mut self, snapshot: &serde_json::Value) -> Result<(), TwinError> {
            self.items = snapshot["items"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Ok(())
        }

        fn seed_from_scenario(
            &mut self,
            initial_state: &serde_json::Value,
        ) -> Result<(), TwinError> {
            // Seed by reading file IDs from the initial_state JSON.
            if let Some(files) = initial_state["files"].as_array() {
                for file in files {
                    if let Some(id) = file["id"].as_str() {
                        self.items.push(id.to_string());
                    }
                }
            }
            Ok(())
        }

        fn evaluate_assertion(
            &self,
            check: &serde_json::Value,
        ) -> Result<AssertionResult, TwinError> {
            let check_type = check["type"].as_str().unwrap_or("");
            match check_type {
                "no_orphans" => Ok(AssertionResult {
                    id: String::new(),
                    passed: true,
                    message: "no orphans (stub)".to_string(),
                }),
                "item_exists" => {
                    let item_id = check["item_id"].as_str().unwrap_or("");
                    let exists = self.items.iter().any(|id| id == item_id);
                    Ok(AssertionResult {
                        id: String::new(),
                        passed: exists,
                        message: if exists {
                            format!("item `{item_id}` exists (stub)")
                        } else {
                            format!("item `{item_id}` missing (stub)")
                        },
                    })
                }
                _ => Err(TwinError::Operation(format!(
                    "unknown assertion type: {check_type}"
                ))),
            }
        }

        fn execute_timeline_action(
            &mut self,
            action: &serde_json::Value,
            _actor_id: &str,
        ) -> Result<TimelineActionResult, TwinError> {
            let action_type = action["type"].as_str().unwrap_or("unknown");
            let endpoint = match action_type {
                "create_file" => "/drive/files",
                "create_folder" => "/drive/folders",
                "set_permission" => "/drive/items/{item_id}/permissions",
                _ => "/unknown",
            };
            // Stub: just track the item name
            if let Some(name) = action["name"].as_str() {
                self.items.push(name.to_string());
            }
            Ok(TimelineActionResult {
                endpoint: endpoint.to_string(),
                response: serde_json::json!({ "ok": true }),
            })
        }

        fn validate_scenario(
            scenario: &serde_json::Value,
        ) -> (Vec<String>, Vec<String>) {
            let mut errors = Vec::new();
            let warnings = Vec::new();

            // Stub: validate that initial_state has files array and includes root.
            if let Some(initial_state) = scenario.get("initial_state") {
                if let Some(files) = initial_state.get("files").and_then(|f| f.as_array()) {
                    let has_root = files
                        .iter()
                        .any(|f| f.get("id").and_then(|v| v.as_str()) == Some("root"));
                    if !has_root && !files.is_empty() {
                        errors.push("initial_state must include `root`".to_string());
                    }
                    // If files is empty, also require root
                    if files.is_empty() {
                        errors.push("initial_state must include `root`".to_string());
                    }
                }
            }

            (errors, warnings)
        }
    }

    fn create_test_dirs() -> (PathBuf, PathBuf) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("twin-server-core-tests-{pid}-{nanos}-{id}"));
        let runs = base.join("runs");
        let scenarios = base.join("scenarios");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::create_dir_all(&scenarios).unwrap();
        (runs, scenarios)
    }

    fn build_test_app() -> Router {
        let (runs, scenarios) = create_test_dirs();
        build_twin_router::<StubTwin>(ServerConfig {
            runs_dir: runs,
            scenarios_dir: scenarios,
            twin_config: TwinConfig {
                seed: 42,
                start_time_unix_ms: 1_704_067_200_000,
            },
            auth: AuthConfig {
                actors: HashMap::new(),
                reject_unauthenticated: false,
            },
        })
    }

    // --- unit tests ---

    #[test]
    fn deterministic_probability_is_stable() {
        let a = deterministic_probability(42, 1, "/drive/files", "alice", "f1");
        let b = deterministic_probability(42, 1, "/drive/files", "alice", "f1");
        assert_eq!(a, b);
    }

    #[test]
    fn deterministic_probability_differs_with_different_inputs() {
        let a = deterministic_probability(42, 1, "/drive/files", "alice", "f1");
        let b = deterministic_probability(42, 2, "/drive/files", "alice", "f1");
        assert_ne!(a, b);
    }

    #[test]
    fn snapshot_hash_is_deterministic() {
        let state = twin_kernel::TwinState {
            revision: 5,
            metadata: BTreeMap::new(),
            service_state: serde_json::json!({ "foo": "bar" }),
        };
        let h1 = snapshot_hash(&state);
        let h2 = snapshot_hash(&state);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16); // 16 hex chars
    }

    #[test]
    fn is_safe_token_accepts_valid_tokens() {
        assert!(is_safe_token("run_000001"));
        assert!(is_safe_token("abc-def_123"));
        assert!(!is_safe_token(""));
        assert!(!is_safe_token("foo/bar"));
        assert!(!is_safe_token("../etc"));
    }

    #[test]
    fn run_store_persist_and_load() {
        let (runs_dir, _) = create_test_dirs();
        let store = RunStore::new(runs_dir).unwrap();
        let artifact = RunArtifact {
            report: ScenarioRunReport {
                run_id: "run_000001".to_string(),
                source_run_id: None,
                scenario_name: "test".to_string(),
                status: "ok".to_string(),
                seed: 42,
                start_time_unix_ms: 1000,
                events_applied: 0,
                timeline_outcomes: vec![],
                fault_hits: vec![],
                assertions: vec![],
                failed_assertions: 0,
                final_snapshot_revision: 1,
                final_snapshot_hash: "abc123".to_string(),
                error: None,
            },
            scenario: ScenarioDocument {
                version: 1,
                name: "test".to_string(),
                seed: 42,
                start_time_unix_ms: 1000,
                actors: vec![],
                initial_state: serde_json::json!({"files": []}),
                timeline: vec![],
                faults: vec![],
                assertions: vec![],
            },
            events: vec![],
            snapshot: twin_kernel::TwinState {
                revision: 1,
                metadata: BTreeMap::new(),
                service_state: serde_json::Value::Null,
            },
        };
        store.persist_run(&artifact).unwrap();
        let loaded = store.load_run("run_000001").unwrap();
        assert_eq!(loaded.report.run_id, "run_000001");
        assert_eq!(loaded.report.scenario_name, "test");
    }

    #[test]
    fn run_store_list_reports() {
        let (runs_dir, _) = create_test_dirs();
        let store = RunStore::new(runs_dir).unwrap();

        let mut artifact = RunArtifact {
            report: ScenarioRunReport {
                run_id: "run_000001".to_string(),
                source_run_id: None,
                scenario_name: "s1".to_string(),
                status: "ok".to_string(),
                seed: 1,
                start_time_unix_ms: 1000,
                events_applied: 0,
                timeline_outcomes: vec![],
                fault_hits: vec![],
                assertions: vec![],
                failed_assertions: 0,
                final_snapshot_revision: 1,
                final_snapshot_hash: "aaa".to_string(),
                error: None,
            },
            scenario: ScenarioDocument {
                version: 1,
                name: "s1".to_string(),
                seed: 1,
                start_time_unix_ms: 1000,
                actors: vec![],
                initial_state: serde_json::json!({"files": []}),
                timeline: vec![],
                faults: vec![],
                assertions: vec![],
            },
            events: vec![],
            snapshot: twin_kernel::TwinState {
                revision: 1,
                metadata: BTreeMap::new(),
                service_state: serde_json::Value::Null,
            },
        };
        store.persist_run(&artifact).unwrap();

        artifact.report.run_id = "run_000002".to_string();
        artifact.report.scenario_name = "s2".to_string();
        store.persist_run(&artifact).unwrap();

        let reports = store.list_run_reports();
        assert_eq!(reports.len(), 2);
    }

    #[test]
    fn run_store_next_run_id_from_existing() {
        let (runs_dir, _) = create_test_dirs();
        let store = RunStore::new(runs_dir).unwrap();
        assert_eq!(store.next_run_id(), 0);

        let artifact = RunArtifact {
            report: ScenarioRunReport {
                run_id: "run_000005".to_string(),
                source_run_id: None,
                scenario_name: "test".to_string(),
                status: "ok".to_string(),
                seed: 42,
                start_time_unix_ms: 1000,
                events_applied: 0,
                timeline_outcomes: vec![],
                fault_hits: vec![],
                assertions: vec![],
                failed_assertions: 0,
                final_snapshot_revision: 1,
                final_snapshot_hash: "abc".to_string(),
                error: None,
            },
            scenario: ScenarioDocument {
                version: 1,
                name: "test".to_string(),
                seed: 42,
                start_time_unix_ms: 1000,
                actors: vec![],
                initial_state: serde_json::json!({"files": []}),
                timeline: vec![],
                faults: vec![],
                assertions: vec![],
            },
            events: vec![],
            snapshot: twin_kernel::TwinState {
                revision: 1,
                metadata: BTreeMap::new(),
                service_state: serde_json::Value::Null,
            },
        };
        store.persist_run(&artifact).unwrap();
        assert_eq!(store.next_run_id(), 5);
    }

    #[test]
    fn validate_scenario_accepts_valid_doc() {
        let scenario = ScenarioDocument {
            version: 1,
            name: "valid".to_string(),
            seed: 42,
            start_time_unix_ms: 1000,
            actors: vec![twin_scenario::Actor {
                id: "alice".to_string(),
                label: "Alice".to_string(),
            }],
            initial_state: serde_json::json!({
                "files": [{
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": null,
                    "owner_id": "alice",
                    "kind": "Folder"
                }]
            }),
            timeline: vec![],
            faults: vec![],
            assertions: vec![],
        };
        let result = validate_scenario::<StubTwin>(&scenario);
        assert!(result.valid, "expected valid: errors={:?}", result.errors);
    }

    #[test]
    fn validate_scenario_rejects_missing_root() {
        let scenario = ScenarioDocument {
            version: 1,
            name: "no-root".to_string(),
            seed: 42,
            start_time_unix_ms: 1000,
            actors: vec![],
            initial_state: serde_json::json!({"files": []}),
            timeline: vec![],
            faults: vec![],
            assertions: vec![],
        };
        let result = validate_scenario::<StubTwin>(&scenario);
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("root")));
    }

    #[test]
    fn validate_scenario_rejects_unknown_timeline_actor() {
        let scenario = ScenarioDocument {
            version: 1,
            name: "bad-actor".to_string(),
            seed: 42,
            start_time_unix_ms: 1000,
            actors: vec![twin_scenario::Actor {
                id: "alice".to_string(),
                label: "Alice".to_string(),
            }],
            initial_state: serde_json::json!({
                "files": [{
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": null,
                    "owner_id": "alice",
                    "kind": "Folder"
                }]
            }),
            timeline: vec![twin_scenario::TimelineEvent {
                at_ms: 1000,
                actor_id: "ghost".to_string(),
                action: serde_json::json!({
                    "type": "create_folder",
                    "parent_id": "root",
                    "name": "X"
                }),
            }],
            faults: vec![],
            assertions: vec![],
        };
        let result = validate_scenario::<StubTwin>(&scenario);
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("ghost")));
    }

    #[test]
    fn diff_run_artifacts_detects_status_change() {
        let base_artifact = RunArtifact {
            report: ScenarioRunReport {
                run_id: "run_a".to_string(),
                source_run_id: None,
                scenario_name: "test".to_string(),
                status: "ok".to_string(),
                seed: 1,
                start_time_unix_ms: 1000,
                events_applied: 0,
                timeline_outcomes: vec![],
                fault_hits: vec![],
                assertions: vec![],
                failed_assertions: 0,
                final_snapshot_revision: 1,
                final_snapshot_hash: "aaa".to_string(),
                error: None,
            },
            scenario: ScenarioDocument {
                version: 1,
                name: "test".to_string(),
                seed: 1,
                start_time_unix_ms: 1000,
                actors: vec![],
                initial_state: serde_json::json!({"files": []}),
                timeline: vec![],
                faults: vec![],
                assertions: vec![],
            },
            events: vec![],
            snapshot: twin_kernel::TwinState {
                revision: 1,
                metadata: BTreeMap::new(),
                service_state: serde_json::Value::Null,
            },
        };

        let mut other = base_artifact.clone();
        other.report.run_id = "run_b".to_string();
        other.report.status = "error".to_string();

        let diff = diff_run_artifacts(&base_artifact, &other);
        assert!(!diff.status_equal);
        assert_eq!(diff.run_id_a, "run_a");
        assert_eq!(diff.run_id_b, "run_b");
    }

    #[test]
    fn is_deterministic_match_passes_for_equal_runs() {
        let diff = RunDiffResult {
            run_id_a: "a".to_string(),
            run_id_b: "b".to_string(),
            status_a: "ok".to_string(),
            status_b: "ok".to_string(),
            status_equal: true,
            snapshot_hash_a: "x".to_string(),
            snapshot_hash_b: "x".to_string(),
            snapshot_hash_equal: true,
            events_count_a: 3,
            events_count_b: 3,
            events_count_equal: true,
            assertions_changed: vec![],
            timeline_mismatch_indices: vec![],
            fault_hit_count_a: 0,
            fault_hit_count_b: 0,
            fault_hit_count_equal: true,
        };
        assert!(is_deterministic_match(&diff));
    }

    // --- integration tests using the control routes ---

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn control_routes_build_successfully() {
        let app = build_test_app();

        // reset
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/reset")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "seed": 1, "start_time_unix_ms": 1000 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // snapshot
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // events
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn control_seed_populates_state() {
        let app = build_test_app();

        // Seed with two items
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "files": [
                                { "id": "f1" },
                                { "id": "f2" }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");

        // Verify via snapshot that items were seeded
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let items = snapshot["service_state"]["items"]
            .as_array()
            .expect("items should be an array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], "f1");
        assert_eq!(items[1], "f2");
    }

    #[tokio::test]
    async fn control_seed_resets_before_seeding() {
        let app = build_test_app();

        // Seed once
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "files": [{ "id": "old" }] }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Seed again — old data should be gone
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "files": [{ "id": "new" }] }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Snapshot should only have "new"
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let items = snapshot["service_state"]["items"]
            .as_array()
            .expect("items should be an array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], "new");
    }

    #[tokio::test]
    async fn control_seed_with_empty_body() {
        let app = build_test_app();

        // Seed with empty object — should succeed (no items)
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Snapshot should have empty items
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let items = snapshot["service_state"]["items"]
            .as_array()
            .expect("items should be an array");
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn control_seed_rejects_invalid_json_with_field_details() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"files\": [}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "invalid_seed_json");
        assert_eq!(body["error"]["details"][0]["path"], "$");
        assert!(body["error"]["details"][0]["line"].is_number());
        assert!(body["error"]["details"][0]["column"].is_number());
    }

    #[tokio::test]
    async fn control_seed_rejects_non_object_payload_with_field_details() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from("[]"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "invalid_seed_payload");
        assert_eq!(body["error"]["details"][0]["path"], "$");
        assert_eq!(body["error"]["details"][0]["message"], "expected object, got array");
    }

    #[tokio::test]
    async fn scenario_validate_accepts_valid() {
        let app = build_test_app();
        let scenario = serde_json::json!({
            "version": 1,
            "name": "valid",
            "seed": 1,
            "start_time_unix_ms": 1000,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [],
            "faults": [],
            "assertions": []
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/validate")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn scenario_validate_rejects_invalid() {
        let app = build_test_app();
        let scenario = serde_json::json!({
            "version": 1,
            "name": "bad",
            "seed": 1,
            "start_time_unix_ms": 1000,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "ghost", "action": { "type": "create_folder", "parent_id": "root", "name": "X" } }
            ],
            "faults": [],
            "assertions": []
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/validate")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn scenario_apply_and_list_runs() {
        let app = build_test_app();
        let scenario = serde_json::json!({
            "version": 1,
            "name": "apply-test",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "create_folder", "parent_id": "root", "name": "Docs" } }
            ],
            "faults": [],
            "assertions": [
                { "id": "no-orphans", "check": { "type": "no_orphans" } }
            ]
        });

        let apply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(apply.status(), StatusCode::OK);
        let apply_body = to_bytes(apply.into_body(), usize::MAX).await.unwrap();
        let apply_json: serde_json::Value = serde_json::from_slice(&apply_body).unwrap();
        assert_eq!(apply_json["status"], "ok");
        assert!(apply_json["run_id"].as_str().is_some());

        // list runs
        let list = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/scenario/runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let list_body = to_bytes(list.into_body(), usize::MAX).await.unwrap();
        let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
        let runs = list_json["runs"].as_array().unwrap();
        assert!(!runs.is_empty());
    }

    #[tokio::test]
    async fn scenario_apply_with_fault_http_error() {
        let app = build_test_app();
        let scenario = serde_json::json!({
            "version": 1,
            "name": "fault-test",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "create_folder", "parent_id": "root", "name": "Blocked" } }
            ],
            "faults": [
                {
                    "id": "force-429",
                    "when": { "endpoint": "create_folder", "actor_id": "alice", "probability": 1.0 },
                    "effect": { "type": "http_error", "status": 429, "message": "quota exceeded" }
                }
            ],
            "assertions": []
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn snapshot_includes_service_state_and_restore_round_trips() {
        let app = build_test_app();

        // Apply a scenario that seeds items into the stub twin.
        let scenario = serde_json::json!({
            "version": 1,
            "name": "snapshot-test",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "doc1", "name": "Readme", "parent_id": "root", "owner_id": "alice", "kind": "File" }
                ]
            },
            "timeline": [],
            "faults": [],
            "assertions": []
        });

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Take a snapshot — it should include service state with seeded items.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snapshot: twin_kernel::TwinState = serde_json::from_slice(&body).unwrap();

        // service_state must be non-null and contain the seeded items.
        assert_ne!(
            snapshot.service_state,
            serde_json::Value::Null,
            "snapshot should include service state"
        );
        let items = snapshot.service_state["items"].as_array().expect("items array");
        assert!(
            items.iter().any(|v| v.as_str() == Some("root")),
            "service_state should contain 'root'"
        );
        assert!(
            items.iter().any(|v| v.as_str() == Some("doc1")),
            "service_state should contain 'doc1'"
        );

        // Reset, then restore from the snapshot.
        let _reset = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/reset")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "seed": 1, "start_time_unix_ms": 0 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        let restore_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/restore")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&snapshot).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(restore_resp.status(), StatusCode::OK);

        // Take a second snapshot — it should match the first.
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let restored: twin_kernel::TwinState = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            snapshot.service_state, restored.service_state,
            "service state should survive snapshot/restore round-trip"
        );
    }

    // -- EnvConfig tests --

    /// Helper: build an `EnvConfig` from a set of key-value pairs.
    fn env_config_from(pairs: &[(&str, &str)]) -> EnvConfig {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EnvConfig::from_env_with(move |key| {
            map.get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        })
    }

    #[test]
    fn env_config_defaults() {
        let cfg = env_config_from(&[]);
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.server.scenarios_dir, cwd.join("scenarios"));
        assert_eq!(cfg.server.runs_dir, cwd.join("runs"));
    }

    #[test]
    fn env_config_overrides_port() {
        let cfg = env_config_from(&[("TWIN_PORT", "3000")]);
        assert_eq!(cfg.port, 3000);
    }

    #[test]
    fn env_config_invalid_port_falls_back_to_default() {
        let cfg = env_config_from(&[("TWIN_PORT", "not-a-number")]);
        assert_eq!(cfg.port, 8080);
    }

    #[test]
    fn env_config_overrides_dirs() {
        let cfg = env_config_from(&[
            ("TWIN_SCENARIOS_DIR", "/tmp/my-scenarios"),
            ("TWIN_RUNS_DIR", "/tmp/my-runs"),
        ]);
        assert_eq!(cfg.server.scenarios_dir, PathBuf::from("/tmp/my-scenarios"));
        assert_eq!(cfg.server.runs_dir, PathBuf::from("/tmp/my-runs"));
    }

    #[test]
    fn env_config_twin_log_level_takes_precedence_over_rust_log() {
        let cfg = env_config_from(&[
            ("TWIN_LOG_LEVEL", "debug"),
            ("RUST_LOG", "trace"),
        ]);
        assert_eq!(cfg.log_level, "debug");
    }

    #[test]
    fn env_config_falls_back_to_rust_log() {
        let cfg = env_config_from(&[("RUST_LOG", "warn")]);
        assert_eq!(cfg.log_level, "warn");
    }

    #[test]
    fn env_config_log_level_default_when_neither_set() {
        let cfg = env_config_from(&[]);
        assert_eq!(cfg.log_level, "info");
    }

    // -- Session management tests --

    /// Helper to parse JSON from a response body.
    async fn json_body(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn session_create_returns_session_id() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "test session"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert!(body["session_id"].as_str().unwrap().starts_with("sess_"));
    }

    #[tokio::test]
    async fn session_create_without_name() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert!(body["session_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn session_create_with_seed() {
        let app = build_test_app();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "seeded",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let session_id = body["session_id"].as_str().unwrap().to_string();

        // Verify seed data is in the snapshot
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let snap = json_body(response).await;
        let items = snap["service_state"]["items"].as_array().unwrap();
        assert!(items.iter().any(|v| v.as_str() == Some("root")));
    }

    #[tokio::test]
    async fn session_get_metadata() {
        let app = build_test_app();
        // Create session
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "meta test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let create_body = json_body(response).await;
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Get session
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["session_id"].as_str().unwrap(), session_id);
        assert_eq!(body["name"].as_str().unwrap(), "meta test");
        assert_eq!(body["status"], "active");
        assert!(body["created_at_unix_ms"].as_i64().is_some());
        assert_eq!(body["event_count"], 0);
    }

    #[tokio::test]
    async fn session_get_not_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/sessions/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn session_end_and_freeze() {
        let app = build_test_app();
        // Create session
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "end test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let create_body = json_body(response).await;
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // End session
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/control/sessions/{session_id}/end"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["session_id"].as_str().unwrap(), session_id);

        // Get session — should be ended
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["status"], "ended");
        assert!(body["ended_at_unix_ms"].as_i64().is_some());

        // Ending again should fail with CONFLICT
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/control/sessions/{session_id}/end"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn session_events_filtering() {
        let app = build_test_app();

        // Create a session (resets state, so events are cleared)
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "events test",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let create_body = json_body(response).await;
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Apply a scenario that generates events (the session is active, so events get tagged)
        let scenario = serde_json::json!({
            "version": 1,
            "name": "event-gen",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "create_folder", "parent_id": "root", "name": "Docs" } }
            ],
            "faults": [],
            "assertions": []
        });
        let _apply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Get session events — should only contain events for this session
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        // All returned events should have our session_id
        for event in &events {
            assert_eq!(event["session_id"].as_str(), Some(session_id.as_str()));
        }

        // Events for non-existent session should return 404
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/sessions/nonexistent/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn session_snapshot_active_and_ended() {
        let app = build_test_app();

        // Create session with seed data
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "snapshot test",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let create_body = json_body(response).await;
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Snapshot while active — should return current state
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let active_snap = json_body(response).await;
        assert!(active_snap["service_state"]["items"].as_array().is_some());

        // End session
        let _end = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/control/sessions/{session_id}/end"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Snapshot after ended — should return frozen snapshot
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ended_snap = json_body(response).await;
        // The frozen snapshot should have the same service state as when active
        assert_eq!(
            active_snap["service_state"], ended_snap["service_state"],
            "frozen snapshot should match state at time of ending"
        );
    }

    #[tokio::test]
    async fn session_events_tagged_with_session_id() {
        let app = build_test_app();

        // Create a session
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "tag test",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let create_body = json_body(response).await;
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Apply scenario (events will be tagged with session_id)
        let scenario = serde_json::json!({
            "version": 1,
            "name": "tag-test",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "create_folder", "parent_id": "root", "name": "A" } },
                { "at_ms": 2000, "actor_id": "alice", "action": { "type": "create_file", "parent_id": "root", "name": "B" } }
            ],
            "faults": [],
            "assertions": []
        });
        let _apply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Check all events from /control/events
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        // Events from the scenario timeline should have the session_id
        assert!(!events.is_empty(), "should have events");
        for event in &events {
            assert_eq!(
                event["session_id"].as_str(),
                Some(session_id.as_str()),
                "event should be tagged with session_id"
            );
        }
    }

    #[tokio::test]
    async fn session_snapshot_not_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/sessions/nonexistent/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn session_create_resets_state() {
        let app = build_test_app();

        // Seed some data via a scenario apply (without session)
        let scenario = serde_json::json!({
            "version": 1,
            "name": "pre-session",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "doc1", "name": "Old Doc", "parent_id": "root", "owner_id": "alice", "kind": "File" }
                ]
            },
            "timeline": [],
            "faults": [],
            "assertions": []
        });
        let _apply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Create session — this should reset state
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let create_body = json_body(response).await;
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Snapshot should be clean (no items from before)
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let snap = json_body(response).await;
        let items = snap["service_state"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(items.is_empty(), "session creation should have reset state");
    }

    #[tokio::test]
    async fn frozen_events_survive_session_reset() {
        let app = build_test_app();

        // Create session 1 with seed data
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "session 1",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        let session1_id = body["session_id"].as_str().unwrap().to_string();

        // Generate events in session 1 via scenario apply
        let scenario = serde_json::json!({
            "version": 1,
            "name": "gen-events",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "create_folder", "parent_id": "root", "name": "Docs" } }
            ],
            "faults": [],
            "assertions": []
        });
        let _apply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Check events before ending — should have events
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let events_before: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(!events_before.is_empty(), "session 1 should have events before end");

        // End session 1
        let _end = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/control/sessions/{session1_id}/end"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Create session 2 — this resets the kernel, clearing live events
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "session 2"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Session 1's events should still be available from frozen_events
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let events_after: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            events_before.len(),
            events_after.len(),
            "frozen events should survive kernel reset"
        );
        for event in &events_after {
            assert_eq!(event["session_id"].as_str(), Some(session1_id.as_str()));
        }
    }

    #[tokio::test]
    async fn list_sessions_endpoint() {
        let app = build_test_app();

        // Initially empty
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let sessions = body["sessions"].as_array().unwrap();
        assert!(sessions.is_empty());

        // Create two sessions
        let _s1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "first"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        let _s2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "second"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // List should contain both sessions
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        // First session should have been auto-ended
        let statuses: Vec<&str> = sessions
            .iter()
            .map(|s| s["status"].as_str().unwrap())
            .collect();
        assert!(statuses.contains(&"ended"), "first session should be auto-ended");
        assert!(statuses.contains(&"active"), "second session should be active");
    }

    #[tokio::test]
    async fn auto_end_active_session_on_new_session() {
        let app = build_test_app();

        // Create session 1
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "auto-end test 1",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        let session1_id = body["session_id"].as_str().unwrap().to_string();

        // Verify session 1 is active
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        assert_eq!(body["status"], "active");

        // Create session 2 — should auto-end session 1
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "auto-end test 2"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let session2_id = body["session_id"].as_str().unwrap().to_string();

        // Session 1 should now be ended
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        assert_eq!(body["status"], "ended");
        assert!(body["ended_at_unix_ms"].as_i64().is_some());

        // Session 2 should be active
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session2_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        assert_eq!(body["status"], "active");
    }

    #[tokio::test]
    async fn auto_ended_session_has_frozen_snapshot_and_events() {
        let app = build_test_app();

        // Create session 1 with seed + generate events
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "auto-end freeze",
                            "seed": {
                                "files": [
                                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                                ]
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        let session1_id = body["session_id"].as_str().unwrap().to_string();

        // Generate events
        let scenario = serde_json::json!({
            "version": 1,
            "name": "auto-freeze-events",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [{ "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "create_folder", "parent_id": "root", "name": "X" } }
            ],
            "faults": [],
            "assertions": []
        });
        let _apply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Get snapshot and event count while active
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let active_snap = json_body(response).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let active_events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();

        // Create session 2 — auto-ends session 1
        let _s2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "session 2"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Session 1 frozen snapshot should match what was captured while active
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let frozen_snap = json_body(response).await;
        assert_eq!(
            active_snap["service_state"], frozen_snap["service_state"],
            "auto-ended session snapshot should preserve state"
        );

        // Session 1 frozen events should match
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/sessions/{session1_id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let frozen_events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            active_events.len(),
            frozen_events.len(),
            "auto-ended session frozen events should survive reset"
        );
    }

    #[test]
    fn session_store_active_session_helper() {
        let mut store = SessionStore::new();
        assert!(store.active_session().is_none());

        store.create(Some("s1".to_string()), 1000);
        assert!(store.active_session().is_some());
        assert_eq!(store.active_session().unwrap().session_id, "sess_000001");

        // Manually end it
        store.get_mut("sess_000001").unwrap().status = SessionStatus::Ended;
        assert!(store.active_session().is_none());
    }

    // -- Event query filter tests --

    /// Helper: build an app, record several events with known fields, then
    /// return the app router so tests can query `/control/events` with filters.
    async fn build_app_with_events() -> Router {
        let (runs, scenarios) = create_test_dirs();
        let run_store = RunStore::new(runs).unwrap();
        let next_run_id = run_store.next_run_id();

        let runtime = Arc::new(Mutex::new(TwinRuntime::new(
            twin_kernel::TwinKernel::new(TwinConfig {
                seed: 42,
                start_time_unix_ms: 1000,
            }),
            StubTwin::default(),
        )));

        let state = AppState::<StubTwin> {
            runtime: runtime.clone(),
            scenario: Arc::new(Mutex::new(ScenarioRuntime::new(next_run_id))),
            run_store: Arc::new(Mutex::new(run_store)),
            session_store: Arc::new(Mutex::new(SessionStore::new())),
            scenarios_dir: scenarios,
        };

        // Record events with known, diverse fields for filter testing.
        // logical_time_unix_ms = start_time(1000) + revision, so we bump
        // revision via set_metadata before each record_event to get
        // predictable, distinct timestamps.
        {
            let mut rt = runtime.lock().await;
            // Event 1: alice, /drive/files, ok, create_file, time=1001, no session
            rt.kernel.set_metadata("t", "1");
            rt.kernel.record_event("/drive/files", Some("alice".to_string()), "ok", "create_file", None);
            // Event 2: bob, /drive/folders, ok, create_folder, time=1002, no session
            rt.kernel.set_metadata("t", "2");
            rt.kernel.record_event("/drive/folders", Some("bob".to_string()), "ok", "create_folder", None);
            // Event 3: alice, /drive/folders, error, create_folder, time=1003, no session
            rt.kernel.set_metadata("t", "3");
            rt.kernel.record_event("/drive/folders", Some("alice".to_string()), "error", "create_folder", None);
            // Event 4: alice, /drive/items/x/permissions, ok, set_permission, time=1004, no session
            rt.kernel.set_metadata("t", "4");
            rt.kernel.record_event("/drive/items/x/permissions", Some("alice".to_string()), "ok", "set_permission", None);

            // Set an active session for the remaining events
            rt.kernel.set_active_session("sess_001".to_string());
            // Event 5: carol, /drive/files, ok, create_file, time=1005, sess_001
            rt.kernel.set_metadata("t", "5");
            rt.kernel.record_event("/drive/files", Some("carol".to_string()), "ok", "create_file", None);
            // Event 6: alice, /drive/files, ok, create_file, time=1006, sess_001
            rt.kernel.set_metadata("t", "6");
            rt.kernel.record_event("/drive/files", Some("alice".to_string()), "ok", "create_file", None);
            rt.kernel.clear_active_session();
        }

        control_routes(state)
    }

    /// Helper: GET /control/events with a query string and parse JSON response.
    async fn get_events(app: &Router, query: &str) -> Vec<serde_json::Value> {
        let uri = if query.is_empty() {
            "/control/events".to_string()
        } else {
            format!("/control/events?{query}")
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn event_filter_no_params_returns_all() {
        let app = build_app_with_events().await;
        let events = get_events(&app, "").await;
        assert_eq!(events.len(), 6, "no filters should return all 6 events");
    }

    #[tokio::test]
    async fn event_filter_by_actor_id() {
        let app = build_app_with_events().await;

        let events = get_events(&app, "actor_id=alice").await;
        assert_eq!(events.len(), 4, "alice has 4 events");
        for e in &events {
            assert_eq!(e["actor_id"].as_str(), Some("alice"));
        }

        let events = get_events(&app, "actor_id=bob").await;
        assert_eq!(events.len(), 1, "bob has 1 event");
        assert_eq!(events[0]["actor_id"].as_str(), Some("bob"));

        let events = get_events(&app, "actor_id=nobody").await;
        assert_eq!(events.len(), 0, "nobody has 0 events");
    }

    #[tokio::test]
    async fn event_filter_by_endpoint() {
        let app = build_app_with_events().await;

        let events = get_events(&app, "endpoint=/drive/files").await;
        assert_eq!(events.len(), 3, "/drive/files has 3 events");
        for e in &events {
            assert_eq!(e["endpoint"].as_str(), Some("/drive/files"));
        }

        let events = get_events(&app, "endpoint=/drive/folders").await;
        assert_eq!(events.len(), 2, "/drive/folders has 2 events");
    }

    #[tokio::test]
    async fn event_filter_by_action() {
        let app = build_app_with_events().await;

        // "action" maps to the "detail" field on TwinEvent
        let events = get_events(&app, "action=create_file").await;
        assert_eq!(events.len(), 3, "create_file has 3 events");
        for e in &events {
            assert_eq!(e["detail"].as_str(), Some("create_file"));
        }

        let events = get_events(&app, "action=create_folder").await;
        assert_eq!(events.len(), 2, "create_folder has 2 events");

        let events = get_events(&app, "action=set_permission").await;
        assert_eq!(events.len(), 1, "set_permission has 1 event");
    }

    #[tokio::test]
    async fn event_filter_by_outcome() {
        let app = build_app_with_events().await;

        let events = get_events(&app, "outcome=ok").await;
        assert_eq!(events.len(), 5, "5 ok events");

        let events = get_events(&app, "outcome=error").await;
        assert_eq!(events.len(), 1, "1 error event");
        assert_eq!(events[0]["outcome"].as_str(), Some("error"));
    }

    #[tokio::test]
    async fn event_filter_by_session_id() {
        let app = build_app_with_events().await;

        let events = get_events(&app, "session_id=sess_001").await;
        assert_eq!(events.len(), 2, "sess_001 has 2 events");
        for e in &events {
            assert_eq!(e["session_id"].as_str(), Some("sess_001"));
        }
    }

    #[tokio::test]
    async fn event_filter_by_time_range() {
        let app = build_app_with_events().await;
        // Events have logical_time_unix_ms = 1001..1006 (start_time + revision).
        // `after` is exclusive lower bound, `before` is exclusive upper bound.

        // after=1003 → events with time > 1003 → 1004, 1005, 1006 → 3 events
        let events = get_events(&app, "after=1003").await;
        assert_eq!(events.len(), 3, "after=1003 should yield 3 events");
        for e in &events {
            assert!(e["logical_time_unix_ms"].as_i64().unwrap() > 1003);
        }

        // before=1004 → events with time < 1004 → 1001, 1002, 1003 → 3 events
        let events = get_events(&app, "before=1004").await;
        assert_eq!(events.len(), 3, "before=1004 should yield 3 events");
        for e in &events {
            assert!(e["logical_time_unix_ms"].as_i64().unwrap() < 1004);
        }

        // Combined: after=1002 & before=1005 → 1003, 1004 → 2 events
        let events = get_events(&app, "after=1002&before=1005").await;
        assert_eq!(events.len(), 2, "after=1002&before=1005 should yield 2 events");
    }

    #[tokio::test]
    async fn event_filter_limit() {
        let app = build_app_with_events().await;

        let events = get_events(&app, "limit=2").await;
        assert_eq!(events.len(), 2, "limit=2 should return 2 events");

        let events = get_events(&app, "limit=0").await;
        assert_eq!(events.len(), 0, "limit=0 should return 0 events");

        let events = get_events(&app, "limit=100").await;
        assert_eq!(events.len(), 6, "limit=100 should return all 6 events");
    }

    #[tokio::test]
    async fn event_filter_combined() {
        let app = build_app_with_events().await;

        // alice + ok + create_file → events 1, 6 → 2 events
        let events = get_events(&app, "actor_id=alice&outcome=ok&action=create_file").await;
        assert_eq!(events.len(), 2, "alice+ok+create_file should yield 2 events");

        // alice + /drive/folders → events 3 → 1 event
        let events = get_events(&app, "actor_id=alice&endpoint=/drive/folders").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"].as_str(), Some("error"));

        // action=create_file + session_id=sess_001 → events 5, 6 → 2
        let events = get_events(&app, "action=create_file&session_id=sess_001").await;
        assert_eq!(events.len(), 2);

        // Combined with limit: alice + ok → 3 events, limit=1 → 1
        let events = get_events(&app, "actor_id=alice&outcome=ok&limit=1").await;
        assert_eq!(events.len(), 1, "combined with limit should truncate");

        // Combined with time range: alice + after=1003 → events 4, 6 → 2
        let events = get_events(&app, "actor_id=alice&after=1003").await;
        assert_eq!(events.len(), 2);
    }

    // -----------------------------------------------------------------------
    // AuthConfig unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn auth_resolve_x_twin_actor_id_header_takes_priority() {
        let auth = AuthConfig {
            actors: HashMap::from([("tok_alice".to_string(), "alice".to_string())]),
            reject_unauthenticated: false,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-Twin-Actor-Id", "bob".parse().unwrap());
        headers.insert("Authorization", "Bearer tok_alice".parse().unwrap());
        assert_eq!(auth.resolve_actor(&headers), Some("bob".to_string()));
    }

    #[test]
    fn auth_resolve_bearer_token_mapped() {
        let auth = AuthConfig {
            actors: HashMap::from([("tok_alice".to_string(), "alice".to_string())]),
            reject_unauthenticated: false,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Bearer tok_alice".parse().unwrap());
        assert_eq!(auth.resolve_actor(&headers), Some("alice".to_string()));
    }

    #[test]
    fn auth_resolve_bearer_token_unmapped_uses_hash_fallback() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: false,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Bearer unknown_token".parse().unwrap());
        let result = auth.resolve_actor(&headers).unwrap();
        assert!(
            result.starts_with("actor_"),
            "hash-based fallback should start with 'actor_': got {result}"
        );
        assert_eq!(result.len(), "actor_".len() + 16, "should be actor_ + 16 hex chars");
    }

    #[test]
    fn auth_resolve_hash_fallback_is_deterministic() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: false,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Bearer my_token_123".parse().unwrap());
        let a = auth.resolve_actor(&headers);
        let b = auth.resolve_actor(&headers);
        assert_eq!(a, b, "same token should always produce the same actor_id");
    }

    #[test]
    fn auth_resolve_different_tokens_produce_different_hashes() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: false,
        };
        let mut h1 = axum::http::HeaderMap::new();
        h1.insert("Authorization", "Bearer token_aaa".parse().unwrap());
        let mut h2 = axum::http::HeaderMap::new();
        h2.insert("Authorization", "Bearer token_bbb".parse().unwrap());
        assert_ne!(
            auth.resolve_actor(&h1),
            auth.resolve_actor(&h2),
            "different tokens should produce different actor IDs"
        );
    }

    #[test]
    fn auth_resolve_no_headers_returns_default() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: false,
        };
        let headers = axum::http::HeaderMap::new();
        assert_eq!(auth.resolve_actor(&headers), Some("default".to_string()));
    }

    #[test]
    fn auth_resolve_empty_bearer_returns_default() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: false,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Bearer ".parse().unwrap());
        assert_eq!(
            auth.resolve_actor(&headers),
            Some("default".to_string()),
            "empty bearer token should fall through to default"
        );
    }

    #[test]
    fn auth_resolve_non_bearer_auth_returns_default() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: false,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert_eq!(
            auth.resolve_actor(&headers),
            Some("default".to_string()),
            "non-Bearer auth should fall through to default"
        );
    }

    #[test]
    fn auth_reject_unauthenticated_returns_none() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: true,
        };
        let headers = axum::http::HeaderMap::new();
        assert_eq!(
            auth.resolve_actor(&headers),
            None,
            "unauthenticated request should be rejected when reject_unauthenticated is true"
        );
    }

    #[test]
    fn auth_reject_unauthenticated_still_allows_bearer() {
        let auth = AuthConfig {
            actors: HashMap::from([("tok".to_string(), "alice".to_string())]),
            reject_unauthenticated: true,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Bearer tok".parse().unwrap());
        assert_eq!(
            auth.resolve_actor(&headers),
            Some("alice".to_string()),
            "authenticated request should still resolve even with reject_unauthenticated"
        );
    }

    #[test]
    fn auth_reject_unauthenticated_still_allows_x_twin_actor_id() {
        let auth = AuthConfig {
            actors: HashMap::new(),
            reject_unauthenticated: true,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-Twin-Actor-Id", "bob".parse().unwrap());
        assert_eq!(
            auth.resolve_actor(&headers),
            Some("bob".to_string()),
            "X-Twin-Actor-Id should work even with reject_unauthenticated"
        );
    }

    #[test]
    fn auth_config_from_file_missing_returns_empty() {
        let auth = AuthConfig::from_file(std::path::Path::new("/nonexistent/actors.json"));
        assert!(auth.actors.is_empty());
    }

    #[test]
    fn auth_config_from_file_loads_actors() {
        let dir = std::env::temp_dir().join(format!(
            "twin-auth-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actors.json");
        std::fs::write(
            &path,
            r#"{ "tok_a": "alice", "tok_b": "bob" }"#,
        )
        .unwrap();
        let auth = AuthConfig::from_file(&path);
        assert_eq!(auth.actors.len(), 2);
        assert_eq!(auth.actors.get("tok_a").unwrap(), "alice");
        assert_eq!(auth.actors.get("tok_b").unwrap(), "bob");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_config_from_file_invalid_json_returns_empty() {
        let dir = std::env::temp_dir().join(format!(
            "twin-auth-bad-json-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actors.json");
        std::fs::write(&path, "NOT JSON").unwrap();
        let auth = AuthConfig::from_file(&path);
        assert!(auth.actors.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_config_auth_file_default_path() {
        // When TWIN_AUTH_FILE is not set, the default path is ./actors.json
        // We can't easily test the exact path without controlling cwd,
        // but we verify the config builds without error.
        let config = EnvConfig::from_env_with(|_key| Err(std::env::VarError::NotPresent));
        // Auth should be an empty map since the file won't exist
        assert!(config.server.auth.actors.is_empty());
    }

    // -- Event recording middleware tests --

    /// Stub twin that exposes a simple data-plane route for middleware testing.
    #[derive(Default, Serialize, Deserialize)]
    struct RoutedStubTwin {
        counter: u32,
    }

    impl TwinService for RoutedStubTwin {
        fn routes(shared: twin_service::SharedTwinState<Self>) -> Router {
            async fn stub_handler(
                State(state): State<twin_service::SharedTwinState<RoutedStubTwin>>,
            ) -> impl IntoResponse {
                let mut rt = state.lock().await;
                rt.service.counter += 1;
                (StatusCode::OK, Json(serde_json::json!({ "count": rt.service.counter })))
            }

            async fn stub_fail_handler() -> impl IntoResponse {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "not found" })),
                )
            }

            Router::new()
                .route("/api/action", post(stub_handler))
                .route("/api/missing", get(stub_fail_handler))
                .with_state(shared)
        }

        fn seed_from_scenario(&mut self, _: &serde_json::Value) -> Result<(), TwinError> {
            Ok(())
        }

        fn evaluate_assertion(&self, _: &serde_json::Value) -> Result<AssertionResult, TwinError> {
            Ok(AssertionResult {
                id: String::new(),
                passed: true,
                message: "stub".to_string(),
            })
        }

        fn execute_timeline_action(
            &mut self,
            _: &serde_json::Value,
            _: &str,
        ) -> Result<TimelineActionResult, TwinError> {
            Ok(TimelineActionResult {
                endpoint: "/api/action".to_string(),
                response: serde_json::json!({}),
            })
        }
    }

    fn build_routed_stub_app() -> Router {
        let (runs, scenarios) = create_test_dirs();
        build_twin_router::<RoutedStubTwin>(ServerConfig {
            runs_dir: runs,
            scenarios_dir: scenarios,
            twin_config: TwinConfig {
                seed: 42,
                start_time_unix_ms: 1_704_067_200_000,
            },
            auth: AuthConfig {
                actors: HashMap::new(),
                reject_unauthenticated: false,
            },
        })
    }

    #[tokio::test]
    async fn data_plane_request_records_event() {
        let app = build_routed_stub_app();

        // Make a data-plane POST request
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/action")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_request_id = response
            .headers()
            .get("X-Twin-Request-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .expect("response should include X-Twin-Request-Id");

        // Now check /control/events — should have exactly one event
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(events.len(), 1, "one data-plane request should produce one event");

        let ev = &events[0];
        assert_eq!(ev["endpoint"], "/api/action");
        assert_eq!(ev["actor_id"], "alice");
        assert_eq!(ev["outcome"], "ok");
        assert_eq!(ev["operation"], "POST");
        assert_eq!(ev["resource"], "/api/action");
        assert_eq!(ev["request_id"], response_request_id);
        assert!(
            ev["detail"].as_str().unwrap().contains("POST"),
            "detail should contain HTTP method"
        );
    }

    #[tokio::test]
    async fn data_plane_event_uses_incoming_correlation_headers() {
        let app = build_routed_stub_app();

        let custom_request_id = "req-custom-123";
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/action")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("X-Request-Id", custom_request_id)
                    .header("traceparent", traceparent)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("X-Twin-Request-Id").unwrap(),
            custom_request_id
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["request_id"], custom_request_id);
        assert_eq!(
            events[0]["trace_id"],
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[tokio::test]
    async fn data_plane_error_records_error_event() {
        let app = build_routed_stub_app();

        // Make a request to a route that returns 404
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/missing")
                    .header("X-Twin-Actor-Id", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Check events
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "error");
        assert_eq!(events[0]["actor_id"], "bob");
    }

    #[tokio::test]
    async fn control_routes_do_not_record_events() {
        let app = build_routed_stub_app();

        // Hit control endpoints — these should NOT produce events
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Check events — should be empty
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            events.len(),
            0,
            "control routes should not produce events"
        );
    }

    #[tokio::test]
    async fn multiple_data_plane_requests_accumulate_events() {
        let app = build_routed_stub_app();

        // Make 3 requests
        for _ in 0..3 {
            let _ = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/action")
                        .header("content-type", "application/json")
                        .header("X-Twin-Actor-Id", "alice")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let events: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(events.len(), 3, "3 data-plane requests should produce 3 events");

        // Verify sequence numbers are increasing
        let seq1 = events[0]["sequence"].as_u64().unwrap();
        let seq2 = events[1]["sequence"].as_u64().unwrap();
        let seq3 = events[2]["sequence"].as_u64().unwrap();
        assert!(seq1 < seq2 && seq2 < seq3, "sequence numbers should be strictly increasing");
    }
}
