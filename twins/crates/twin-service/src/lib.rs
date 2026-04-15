use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use twin_kernel::{TwinConfig, TwinKernel, TwinState};

// Re-export derive macro so twins can `use twin_service::TwinSnapshot;`
pub use twin_macros::TwinSnapshot;

#[derive(Debug, Error)]
pub enum TwinError {
    #[error("service operation failed: {0}")]
    Operation(String),
}

pub struct TwinRuntime<TService> {
    pub kernel: TwinKernel,
    pub service: TService,
}

impl<TService> TwinRuntime<TService> {
    pub fn new(kernel: TwinKernel, service: TService) -> Self {
        Self { kernel, service }
    }
}

impl<TService: Default> TwinRuntime<TService> {
    pub fn reset(&mut self, config: TwinConfig) {
        self.kernel.reset(config);
        self.service = TService::default();
    }

    pub fn snapshot(&self) -> TwinState {
        self.kernel.snapshot()
    }

    pub fn restore(&mut self, snapshot: TwinState) {
        self.kernel.restore(snapshot);
    }
}

// ---------------------------------------------------------------------------
// Resolved actor identity (shared across twin-server-core + twins)
// ---------------------------------------------------------------------------

/// Resolved actor identity injected into each request by the auth middleware.
///
/// The middleware in `twin-server-core` examines `X-Twin-Actor-Id` and
/// `Authorization: Bearer <token>` headers, maps them through `AuthConfig`,
/// and inserts this value as an Axum `Extension`.  Individual twin route
/// handlers extract it via `Extension<ResolvedActorId>`.
#[derive(Debug, Clone)]
pub struct ResolvedActorId(pub String);

// ---------------------------------------------------------------------------
// New TwinService abstraction (Phase 1A)
// ---------------------------------------------------------------------------

/// Shared handle to a `TwinRuntime<T>` suitable for embedding in Axum handlers.
pub type SharedTwinState<T> = Arc<Mutex<TwinRuntime<T>>>;

/// Result of evaluating a single assertion check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    pub id: String,
    pub passed: bool,
    pub message: String,
}

/// Result returned by [`TwinService::execute_timeline_action`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineActionResult {
    /// The endpoint string that identifies this action (e.g. "/drive/files").
    pub endpoint: String,
    /// The JSON-serialised response from the twin for this action.
    pub response: serde_json::Value,
}

/// Metadata for a single API method in a discovery document resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryMethod {
    /// Fully-qualified method ID, e.g. "drive.files.list".
    pub id: String,
    /// HTTP method (GET, POST, PUT, PATCH, DELETE).
    pub http_method: String,
    /// Path template relative to `servicePath`, e.g. "files" or "files/{fileId}".
    pub path: String,
    /// Short description of the method.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Named path/query parameters.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, serde_json::Value>,
    /// Parameter ordering hint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameter_order: Vec<String>,
    /// Whether this method supports media upload.
    #[serde(default)]
    pub supports_media_upload: bool,
    /// Media upload configuration (only when `supports_media_upload` is true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_upload: Option<serde_json::Value>,
    /// Request body schema reference, e.g. `{"$ref": "File"}`.
    /// When present, the SDK knows the method accepts a JSON request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<serde_json::Value>,
    /// Response schema reference, e.g. `{"$ref": "File"}`.
    /// When present, the SDK knows the method returns JSON and will parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
}

/// A named group of methods (a "resource") in a discovery document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResource {
    /// The methods in this resource, keyed by short name (e.g. "list", "get").
    pub methods: BTreeMap<String, DiscoveryMethod>,
    /// Nested sub-resources (e.g. "permissions" under "files").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, DiscoveryResource>,
}

/// Static discovery metadata provided by a twin.
///
/// The framework uses this to build a Google-compatible discovery document
/// at runtime, injecting `rootUrl` from the request's `Host` header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryMeta {
    /// API name, e.g. "drive" or "gmail".
    pub name: String,
    /// API version, e.g. "v3" or "v1".
    pub version: String,
    /// Human-readable title, e.g. "Google Drive API".
    pub title: String,
    /// Short description.
    pub description: String,
    /// Service path prefix, e.g. "drive/v3/" or "gmail/v1/users/me/".
    pub service_path: String,
    /// Top-level resources with their methods.
    pub resources: BTreeMap<String, DiscoveryResource>,
    /// JSON schemas (can be empty `{}`).
    #[serde(default)]
    pub schemas: serde_json::Value,
}

/// Core trait that every digital-twin service must implement.
///
/// The framework calls these methods to wire up HTTP routes, persist and
/// restore state, seed scenarios, and evaluate assertions.
pub trait TwinService:
    Sized + Send + Sync + Default + Serialize + DeserializeOwned + 'static
{
    /// Build Axum routes for this twin (compatibility endpoints, state
    /// inspection, etc.). Called once at startup; the shared state handle
    /// is provided so handlers can clone the `Arc` and access the runtime.
    fn routes(shared: SharedTwinState<Self>) -> Router;

    /// Return discovery metadata for this twin, if it mimics a Google API.
    ///
    /// The framework serves this as a Google API discovery document at
    /// standard discovery URLs (`/$discovery/rest?version=...` and
    /// `/discovery/v1/apis/{api}/{version}/rest`).
    ///
    /// Default: `None` (no discovery document served).
    fn discovery_meta() -> Option<DiscoveryMeta> {
        None
    }

    /// Serialize the twin's domain state for snapshot.
    ///
    /// Default: serialise `self` via serde_json.
    fn service_snapshot(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("TwinService::service_snapshot default serialize failed")
    }

    /// Restore the twin's domain state from a snapshot.
    ///
    /// Default: deserialise from JSON and replace `self`.
    fn service_restore(&mut self, snapshot: &serde_json::Value) -> Result<(), TwinError> {
        *self = serde_json::from_value(snapshot.clone()).map_err(|e| {
            TwinError::Operation(format!("service_restore deserialize failed: {e}"))
        })?;
        Ok(())
    }

    /// Seed initial state from a scenario document (generic JSON).
    fn seed_from_scenario(&mut self, initial_state: &serde_json::Value) -> Result<(), TwinError>;

    /// Evaluate a single assertion check (generic JSON in, result out).
    /// Returns `Err` for malformed or unrecognized check payloads.
    fn evaluate_assertion(&self, check: &serde_json::Value) -> Result<AssertionResult, TwinError>;

    /// Execute a timeline action from a scenario.
    ///
    /// The `action` is the JSON-serialised `Action` from the scenario
    /// document. The twin deserialises and executes it, returning the
    /// endpoint string (for fault matching) and a JSON response.
    fn execute_timeline_action(
        &mut self,
        action: &serde_json::Value,
        actor_id: &str,
    ) -> Result<TimelineActionResult, TwinError>;

    /// Validate domain-specific parts of a scenario document.
    ///
    /// The server-core validates generic structural properties (version,
    /// name, actors, faults, timeline actor references). This method is
    /// called to validate domain-specific concerns like initial_state
    /// contents, action payloads, and assertion check variants.
    ///
    /// Returns `(errors, warnings)` — the server-core merges these into
    /// the overall validation result. Default: no domain-specific
    /// validation (empty errors and warnings).
    fn validate_scenario(_scenario: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    /// Reset to default empty state.
    ///
    /// Default: replace `self` with `Self::default()`.
    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// State inspection abstraction
// ---------------------------------------------------------------------------

/// A generic node for state inspection responses.
///
/// Twins that implement [`StateInspectable`] convert their domain-specific
/// state items into this generic shape.  The framework provides HTTP route
/// handlers that serve these nodes via `/state/items`, `/state/items/:id`,
/// and `/state/tree`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateNode {
    /// Unique identifier for this node.
    pub id: String,
    /// Human-readable label (e.g. file name).
    pub label: String,
    /// Node kind (e.g. "file", "folder", "message", "channel").
    pub kind: String,
    /// Parent node ID, if any.  Used to build the tree view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Additional domain-specific properties (permissions, size, etc.).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, serde_json::Value>,
}

/// Trait for twins that support generic state inspection.
///
/// This is opt-in — twins that implement this trait can use the framework's
/// [`state_inspection_routes`] helper to get `/state/items`, `/state/items/:id`,
/// and `/state/tree` endpoints automatically.
pub trait StateInspectable {
    /// Return all state nodes as a flat list.
    fn inspect_state(&self) -> Vec<StateNode>;

    /// Return a single node by ID, or `None` if not found.
    fn inspect_node(&self, id: &str) -> Option<StateNode>;
}

// ---------------------------------------------------------------------------
// Generic state inspection routes
// ---------------------------------------------------------------------------

/// Build generic state inspection routes for twins that implement
/// [`StateInspectable`].
///
/// Returns an Axum `Router` with these endpoints:
/// - `GET /state/items` — flat list of all state nodes
/// - `GET /state/items/{id}` — single node by ID
/// - `GET /state/tree` — hierarchical tree built from parent_id links
///
/// Twins call this from their [`TwinService::routes`] implementation and
/// merge it into their router.
pub fn state_inspection_routes<T>(_shared: SharedTwinState<T>) -> Router<SharedTwinState<T>>
where
    T: TwinService + StateInspectable,
{
    Router::new()
        .route("/state/items", get(si_route_items::<T>))
        .route("/state/items/{id}", get(si_route_item::<T>))
        .route("/state/tree", get(si_route_tree::<T>))
}

async fn si_route_items<T: TwinService + StateInspectable>(
    State(state): State<SharedTwinState<T>>,
) -> impl IntoResponse {
    let rt = state.lock().await;
    let nodes = rt.service.inspect_state();
    (StatusCode::OK, Json(serde_json::json!({ "items": nodes })))
}

async fn si_route_item<T: TwinService + StateInspectable>(
    State(state): State<SharedTwinState<T>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rt = state.lock().await;
    match rt.service.inspect_node(&id) {
        Some(node) => (
            StatusCode::OK,
            Json(serde_json::json!({ "item": node })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("item '{}' not found", id) })),
        )
            .into_response(),
    }
}

async fn si_route_tree<T: TwinService + StateInspectable>(
    State(state): State<SharedTwinState<T>>,
) -> impl IntoResponse {
    let rt = state.lock().await;
    let nodes = rt.service.inspect_state();

    // Find root nodes (no parent_id).
    let roots: Vec<&StateNode> = nodes.iter().filter(|n| n.parent_id.is_none()).collect();

    if roots.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "no root node found" })),
        )
            .into_response();
    }

    // Build children lookup: parent_id -> Vec<&StateNode>
    let mut children_map: BTreeMap<&str, Vec<&StateNode>> = BTreeMap::new();
    for node in &nodes {
        if let Some(ref pid) = node.parent_id {
            children_map.entry(pid.as_str()).or_default().push(node);
        }
    }

    fn build_tree(
        node: &StateNode,
        full_path: &str,
        children_map: &BTreeMap<&str, Vec<&StateNode>>,
    ) -> serde_json::Value {
        let children: Vec<serde_json::Value> = children_map
            .get(node.id.as_str())
            .map(|kids| {
                kids.iter()
                    .map(|child| {
                        let child_path = format!("{}/{}", full_path, child.label);
                        build_tree(child, &child_path, children_map)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut obj = serde_json::json!({
            "id": node.id,
            "name": node.label,
            "kind": node.kind,
            "full_path": full_path,
            "children": children,
        });

        // Merge properties into the tree node
        if let Some(map) = obj.as_object_mut() {
            for (k, v) in &node.properties {
                map.insert(k.clone(), v.clone());
            }
        }

        obj
    }

    let root = roots[0];
    let tree = build_tree(root, &root.label, &children_map);

    (StatusCode::OK, Json(serde_json::json!({ "root": tree }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct EchoService;

    #[test]
    fn restore_replays_same_state() {
        let mut runtime = TwinRuntime::new(
            TwinKernel::new(TwinConfig {
                seed: 42,
                start_time_unix_ms: 1000,
            }),
            EchoService,
        );

        runtime.kernel.set_metadata("key", "one");
        let snap = runtime.snapshot();
        runtime.kernel.set_metadata("key", "two");

        runtime.restore(snap.clone());
        assert_eq!(runtime.snapshot().revision, snap.revision);
        assert_eq!(
            runtime.snapshot().metadata.get("key"),
            Some(&"one".to_string())
        );
    }

    #[test]
    fn assertion_result_round_trips_through_serde() {
        let result = AssertionResult {
            id: "check_file_exists".to_string(),
            passed: true,
            message: "File found at /inbox/report.pdf".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: AssertionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, result.id);
        assert_eq!(restored.passed, result.passed);
        assert_eq!(restored.message, result.message);
    }
}
