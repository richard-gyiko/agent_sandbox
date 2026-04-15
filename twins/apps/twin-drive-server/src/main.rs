//! Thin binary for the Google Drive digital twin.
//!
//! Wires [`DriveTwinService`] into the generic twin-server-core host and
//! starts listening.  All control-surface logic (scenarios, runs, fault
//! injection, etc.) is provided by `twin-server-core`.

use std::net::SocketAddr;
use tracing::info;
use twin_drive::DriveTwinService;
use twin_server_core::{EnvConfig, build_twin_router};

#[tokio::main]
async fn main() {
    let env_config = EnvConfig::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&env_config.log_level)
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let port = env_config.port;
    let host = env_config.host;
    info!(?env_config, "resolved configuration");
    let app = build_twin_router::<DriveTwinService>(env_config.server);

    let addr = SocketAddr::from((host, port));
    info!("twin-drive-server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind listener");
    axum::serve(listener, app).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::util::ServiceExt;
    use twin_kernel::TwinConfig;
    use twin_server_core::ServerConfig;

    fn create_test_dirs() -> (std::path::PathBuf, std::path::PathBuf) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base =
            std::env::temp_dir().join(format!("twin-drive-server-tests-{pid}-{nanos}-{id}"));
        let runs = base.join("runs");
        let scenarios = base.join("scenarios");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::create_dir_all(&scenarios).unwrap();
        (runs, scenarios)
    }

    fn build_test_app() -> axum::Router {
        let (runs, scenarios) = create_test_dirs();
        let config = ServerConfig {
            runs_dir: runs,
            scenarios_dir: scenarios,
            twin_config: TwinConfig {
                seed: 42,
                start_time_unix_ms: 1_704_067_200_000,
            },
            auth: twin_server_core::AuthConfig {
                actors: std::collections::HashMap::new(),
                reject_unauthenticated: false,
            },
        };
        build_twin_router::<DriveTwinService>(config)
    }

    #[tokio::test]
    async fn health_returns_ok() {
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
    }

    #[tokio::test]
    async fn drive_routes_are_registered() {
        let app = build_test_app();

        // The Drive twin should register its compatibility routes.
        // Creating a folder should work after reset.
        let app_clone = app.clone();

        // Reset first
        let _reset = app_clone
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/reset")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "seed": 1, "start_time_unix_ms": 1000 })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Create a folder via the Drive compatibility route.
        // After reset, root is owned by "system", so use that actor.
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/folders")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Test Folder",
                            "parent_id": "root",
                            "actor_id": "system"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn control_routes_are_registered() {
        let app = build_test_app();

        // Verify control routes work: snapshot, events, reset, scenario validate.
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
    }

    fn build_test_app_with_auth(actors: std::collections::HashMap<String, String>) -> axum::Router {
        let (runs, scenarios) = create_test_dirs();
        let config = ServerConfig {
            runs_dir: runs,
            scenarios_dir: scenarios,
            twin_config: TwinConfig {
                seed: 42,
                start_time_unix_ms: 1_704_067_200_000,
            },
            auth: twin_server_core::AuthConfig { actors, reject_unauthenticated: false },
        };
        build_twin_router::<DriveTwinService>(config)
    }

    /// Helper: read response body as JSON (used by e2e_sdk_mimicry_with_auth).
    async fn read_body_json(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn e2e_sdk_mimicry_with_auth() {
        // ── 1. Setup: create app with auth config mapping tokens to actors ──
        let mut actors = std::collections::HashMap::new();
        actors.insert("token_alice".to_string(), "alice".to_string());
        actors.insert("token_bob".to_string(), "bob".to_string());
        let app = build_test_app_with_auth(actors);

        // ── 2. Seed initial state via scenario/apply so alice owns root ──
        let scenario = serde_json::json!({
            "version": 1,
            "name": "sdk-mimicry-setup",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [
                { "id": "alice", "label": "Alice" },
                { "id": "bob",   "label": "Bob"   }
            ],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [],
            "faults": [],
            "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK, "scenario apply should succeed");

        // ── 3. Create folder as Alice via v3 (Bearer token) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Alice Docs",
                            "mimeType": "application/vnd.google-apps.folder",
                            "parents": ["root"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "create folder should return 201");
        let folder = read_body_json(resp).await;
        assert_eq!(folder["kind"], "drive#file");
        assert_eq!(folder["name"], "Alice Docs");
        assert_eq!(folder["mimeType"], "application/vnd.google-apps.folder");
        let folder_id = folder["id"].as_str().unwrap().to_string();

        // ── 4. Create file as Alice inside the folder ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "readme.txt",
                            "parents": [folder_id]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "create file should return 201");
        let file = read_body_json(resp).await;
        assert_eq!(file["kind"], "drive#file");
        assert_eq!(file["name"], "readme.txt");
        let file_id = file["id"].as_str().unwrap().to_string();

        // ── 5. List files as Bob — should see nothing (no permissions yet) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("Authorization", "Bearer token_bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = read_body_json(resp).await;
        assert_eq!(list["kind"], "drive#fileList");
        let bob_files = list["files"].as_array().unwrap();
        assert!(
            bob_files.is_empty(),
            "bob should see no files before permission grant, got: {bob_files:?}"
        );

        // ── 6. Add permission for Bob on the folder (Alice grants writer) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/drive/v3/files/{folder_id}/permissions"))
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({
                            "role": "writer",
                            "emailAddress": "bob@example.com"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "add permission should succeed");
        let perm = read_body_json(resp).await;
        assert_eq!(perm["role"], "writer");
        assert_eq!(perm["type"], "user");

        // Also give Bob permission on the file itself so he can update it
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/drive/v3/files/{file_id}/permissions"))
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({
                            "role": "writer",
                            "emailAddress": "bob@example.com"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "add file permission should succeed");

        // ── 7. Update file as Bob (rename readme.txt → README.md) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_bob")
                    .body(Body::from(
                        serde_json::json!({ "name": "README.md" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "update file should succeed");
        let updated = read_body_json(resp).await;
        assert_eq!(updated["name"], "README.md");
        assert_eq!(updated["kind"], "drive#file");

        // ── 8. Get file — verify name changed ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = read_body_json(resp).await;
        assert_eq!(got["name"], "README.md");
        assert_eq!(got["id"], file_id);

        // ── 9. Delete file (Alice) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "delete should return 204");

        // ── 10. Verify deletion — GET should return 404 ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "deleted file should return 404");

        // ── 11. Verify auth middleware: X-Twin-Actor-Id header override ──
        // When X-Twin-Actor-Id is present, it takes priority over Bearer token.
        // Create a file using X-Twin-Actor-Id: alice (bypassing Bearer entirely).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "via-header-override.txt",
                            "parents": ["root"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "X-Twin-Actor-Id override should work without Bearer token"
        );
        let override_file = read_body_json(resp).await;
        assert_eq!(override_file["name"], "via-header-override.txt");

        // Also verify X-Twin-Actor-Id takes priority OVER Bearer token:
        // Send both headers — X-Twin-Actor-Id: alice and Authorization: Bearer token_bob.
        // The request should resolve to alice (not bob), so alice can create under root.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Authorization", "Bearer token_bob")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "priority-test.txt",
                            "parents": ["root"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "X-Twin-Actor-Id should take priority over Bearer token"
        );
        let priority_file = read_body_json(resp).await;
        assert_eq!(priority_file["name"], "priority-test.txt");
    }

    #[tokio::test]
    async fn scenario_apply_with_drive_operations() {
        let app = build_test_app();

        let scenario = serde_json::json!({
            "version": 1,
            "name": "drive-e2e",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": {
                        "type": "create_folder",
                        "parent_id": "root",
                        "name": "Documents"
                    }
                },
                {
                    "at_ms": 2000,
                    "actor_id": "alice",
                    "action": {
                        "type": "create_file",
                        "parent_id": "root",
                        "name": "readme.txt"
                    }
                }
            ],
            "faults": [],
            "assertions": [
                {
                    "id": "no-orphans",
                    "check": { "type": "no_orphans" }
                }
            ]
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
        assert_eq!(response.status(), StatusCode::OK);

        // Parse the response to verify assertions passed.
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(report["status"], "ok", "scenario should pass: {report}");
    }

    /// Helper to read a JSON body from a response.
    async fn json_body(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn e2e_agent_session_workflow() {
        let app = build_test_app();

        // ── Step 1: Create session ──────────────────────────────────────
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "e2e-session",
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
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let session_id = body["session_id"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty(), "session_id should be non-empty");

        // ── Step 2: Apply scenario ──────────────────────────────────────
        let scenario = serde_json::json!({
            "version": 1,
            "name": "e2e-drive-scenario",
            "seed": 42,
            "start_time_unix_ms": 1704067200000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": {
                        "type": "create_folder",
                        "parent_id": "root",
                        "name": "Documents"
                    }
                },
                {
                    "at_ms": 2000,
                    "actor_id": "alice",
                    "action": {
                        "type": "create_file",
                        "parent_id": "root",
                        "name": "notes.txt"
                    }
                }
            ],
            "faults": [],
            "assertions": [
                {
                    "id": "no-orphans",
                    "check": { "type": "no_orphans" }
                }
            ]
        });

        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "ok", "scenario apply should pass: {body}");

        // ── Step 3: Call Drive APIs directly ────────────────────────────
        // Create a folder via POST /drive/folders
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/folders")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Projects",
                            "parent_id": "root",
                            "actor_id": "alice"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let folder_body = json_body(resp).await;
        // DriveResponse::Created is externally tagged: {"Created":{"item":{...}}}
        let folder_item = &folder_body["Created"]["item"];
        let folder_id = folder_item["id"].as_str().unwrap().to_string();
        assert!(!folder_id.is_empty());
        assert_eq!(folder_item["name"], "Projects");

        // Create a file via POST /drive/files
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/files")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "report.pdf",
                            "parent_id": &folder_id,
                            "actor_id": "alice"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let file_body = json_body(resp).await;
        let file_item = &file_body["Created"]["item"];
        let file_id = file_item["id"].as_str().unwrap().to_string();
        assert!(!file_id.is_empty());
        assert_eq!(file_item["name"], "report.pdf");

        // ── Step 4: Call v3 mimicry routes ──────────────────────────────
        // GET /drive/v3/files — list all files
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list_body = json_body(resp).await;
        assert_eq!(list_body["kind"], "drive#fileList");
        let files = list_body["files"].as_array().unwrap();
        assert!(
            files.len() >= 4,
            "should have at least 4 items (Documents, notes.txt, Projects, report.pdf), got {}",
            files.len()
        );

        // GET /drive/v3/files/{id} — get specific file
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let get_body = json_body(resp).await;
        assert_eq!(get_body["kind"], "drive#file");
        assert_eq!(get_body["id"], file_id);
        assert_eq!(get_body["name"], "report.pdf");

        // ── Step 5: Inspect state ───────────────────────────────────────
        // GET /state/items
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/state/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let items_body = json_body(resp).await;
        let items = items_body["items"].as_array().unwrap();
        // root + Documents + notes.txt + Projects + report.pdf = 5
        assert!(
            items.len() >= 5,
            "should have at least 5 items (including root), got {}",
            items.len()
        );

        // GET /state/tree
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/state/tree")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let tree_body = json_body(resp).await;
        assert!(
            tree_body["root"].is_object(),
            "tree should have a root object"
        );

        // ── Step 6: End session ─────────────────────────────────────────
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let end_body = json_body(resp).await;
        assert_eq!(end_body["status"], "ok");
        assert_eq!(end_body["session_id"], session_id);
        let final_event_count = end_body["event_count"].as_u64().unwrap();
        assert!(
            final_event_count > 0,
            "should have recorded events during the session"
        );

        // ── Step 7: Query events with filters ───────────────────────────
        // Filter by session_id
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!(
                        "/control/events?session_id={session_id}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let events: Vec<serde_json::Value> = {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        assert!(
            !events.is_empty(),
            "should have events for this session"
        );
        for ev in &events {
            assert_eq!(
                ev["session_id"].as_str(),
                Some(session_id.as_str()),
                "all events should belong to our session"
            );
        }

        // Filter by endpoint (scenario timeline records events with
        // endpoint="/drive/folders" and detail="timeline action applied")
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/control/events?endpoint=/drive/folders")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let folder_events: Vec<serde_json::Value> = {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        assert!(
            !folder_events.is_empty(),
            "should have /drive/folders events from the scenario timeline"
        );
        for ev in &folder_events {
            assert_eq!(ev["endpoint"], "/drive/folders");
        }

        // ── Step 8: Get session metadata ────────────────────────────────
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        assert_eq!(meta["status"], "ended");
        assert_eq!(meta["session_id"], session_id);
        assert_eq!(meta["name"], "e2e-session");
        assert_eq!(
            meta["event_count"].as_u64().unwrap(),
            final_event_count,
            "frozen event count should match"
        );

        // ── Step 9: List sessions ───────────────────────────────────────
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let sessions = list["sessions"].as_array().unwrap();
        assert!(
            sessions
                .iter()
                .any(|s| s["session_id"].as_str() == Some(&session_id)),
            "our session should appear in the list"
        );

        // ── Step 10: Snapshot ───────────────────────────────────────────
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let snapshot = json_body(resp).await;
        // The snapshot should be a valid JSON object representing the twin state
        assert!(
            snapshot.is_object(),
            "snapshot should be a JSON object"
        );
    }

    // ── AU-1: reject_unauthenticated E2E ───────────────────────────────

    fn build_test_app_reject_unauth() -> axum::Router {
        let (runs, scenarios) = create_test_dirs();
        let mut actors = std::collections::HashMap::new();
        actors.insert("tok_alice".to_string(), "alice".to_string());
        let config = ServerConfig {
            runs_dir: runs,
            scenarios_dir: scenarios,
            twin_config: TwinConfig {
                seed: 42,
                start_time_unix_ms: 1_704_067_200_000,
            },
            auth: twin_server_core::AuthConfig {
                actors,
                reject_unauthenticated: true,
            },
        };
        build_twin_router::<DriveTwinService>(config)
    }

    #[tokio::test]
    async fn reject_unauthenticated_returns_401_for_bare_request() {
        let app = build_test_app_reject_unauth();

        // Request without any auth headers should get 401
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Verify error JSON shape
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], 401);
        assert!(
            body["error"]["message"].as_str().unwrap().contains("Authentication required"),
            "401 body should explain that auth is needed: {body}"
        );
    }

    #[tokio::test]
    async fn reject_unauthenticated_allows_bearer_token() {
        let app = build_test_app_reject_unauth();

        // Seed state first — auth middleware applies to ALL routes, so
        // control routes also need a valid auth header.
        let scenario = serde_json::json!({
            "version": 1, "name": "auth-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/apply")
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer tok_alice")
                    .body(Body::from(scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Request with valid Bearer token should succeed
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("Authorization", "Bearer tok_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Request with X-Twin-Actor-Id should also succeed
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── CC-1: Error response JSON shape ────────────────────────────────

    #[tokio::test]
    async fn error_responses_have_correct_json_shape() {
        let mut actors = std::collections::HashMap::new();
        actors.insert("tok_alice".to_string(), "alice".to_string());
        let app = build_test_app_with_auth(actors);

        // Seed state
        let scenario = serde_json::json!({
            "version": 1, "name": "error-shape-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // 404: GET a non-existent file
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/does-not-exist")
                    .header("Authorization", "Bearer tok_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], 404, "error.code should be 404: {body}");
        assert!(
            body["error"]["message"].as_str().is_some(),
            "error.message should be a string: {body}"
        );

        // 403: Bob tries to access Alice's file
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/root")
                    .header("X-Twin-Actor-Id", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], 403, "error.code should be 403: {body}");
        assert!(
            body["error"]["message"].as_str().is_some(),
            "error.message should be a string: {body}"
        );
    }

    // ── CS-2: Scenario replay + determinism verification ───────────────

    #[tokio::test]
    async fn scenario_replay_determinism_verification() {
        let app = build_test_app();

        // Apply a scenario with timeline actions
        let scenario = serde_json::json!({
            "version": 1,
            "name": "determinism-test",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": { "type": "create_folder", "parent_id": "root", "name": "Documents" }
                },
                {
                    "at_ms": 2000,
                    "actor_id": "alice",
                    "action": { "type": "create_file", "parent_id": "root", "name": "readme.txt" }
                }
            ],
            "faults": [],
            "assertions": [
                { "id": "no-orphans", "check": { "type": "no_orphans" } }
            ]
        });

        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let report = json_body(resp).await;
        assert_eq!(report["status"], "ok");
        let run_id = report["run_id"].as_str().unwrap().to_string();

        // Verify replay determinism
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!("/control/scenario/runs/{run_id}/verify-replay"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let verify = json_body(resp).await;
        assert_eq!(
            verify["ok"], true,
            "replay should be deterministic: {verify}"
        );
        assert_eq!(verify["baseline_run_id"], run_id);
        assert!(
            verify["replay_run_id"].as_str().is_some(),
            "should have a replay_run_id"
        );

        // Also test POST /control/scenario/replay
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/replay")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "run_id": run_id }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let replay_report = json_body(resp).await;
        assert_eq!(replay_report["status"], "ok", "replay should succeed: {replay_report}");
    }

    // ── FI-1: Fault injection E2E with real Drive twin ─────────────────

    #[tokio::test]
    async fn fault_injection_http_error_with_real_twin() {
        let app = build_test_app();

        let scenario = serde_json::json!({
            "version": 1,
            "name": "fault-drive-e2e",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": { "type": "create_folder", "parent_id": "root", "name": "Blocked" }
                }
            ],
            "faults": [
                {
                    "id": "force-503",
                    "when": { "endpoint": "create_folder", "actor_id": "alice", "probability": 1.0 },
                    "effect": { "type": "http_error", "status": 503, "message": "service unavailable" }
                }
            ],
            "assertions": []
        });

        let resp = app
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
        // Fault fires during timeline execution, propagating the HTTP error status
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "fault should produce 503"
        );
        let body = json_body(resp).await;
        assert_eq!(body["error"], "service unavailable");
    }

    // ── CS-3: Snapshot restore round-trip via HTTP ──────────────────────

    #[tokio::test]
    async fn snapshot_restore_round_trip_via_http() {
        let app = build_test_app();

        // Seed state with a scenario
        let scenario = serde_json::json!({
            "version": 1, "name": "restore-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "f1", "name": "doc.txt", "parent_id": "root", "owner_id": "alice", "kind": "File" }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // Take a snapshot
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);
        let snapshot = json_body(resp).await;
        // Verify snapshot has our file
        assert!(
            snapshot["service_state"].to_string().contains("doc.txt"),
            "snapshot should contain doc.txt"
        );

        // Reset to empty state
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/reset")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "seed": 99, "start_time_unix_ms": 1000 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Verify reset cleared state — listing files should show only root
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let files_after_reset = json_body(resp).await;
        let file_list = files_after_reset["files"].as_array().unwrap();
        assert!(
            !file_list.iter().any(|f| f["name"] == "doc.txt"),
            "doc.txt should be gone after reset"
        );

        // Restore from snapshot
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/restore")
                    .header("content-type", "application/json")
                    .body(Body::from(snapshot.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let restore_body = json_body(resp).await;
        assert_eq!(restore_body["status"], "ok");

        // Verify state is restored — doc.txt should be back
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let files_after_restore = json_body(resp).await;
        let file_list = files_after_restore["files"].as_array().unwrap();
        assert!(
            file_list.iter().any(|f| f["name"] == "doc.txt"),
            "doc.txt should be restored: {files_after_restore}"
        );
    }

    // ── CS-4: Scenario validate endpoint ───────────────────────────────

    #[tokio::test]
    async fn scenario_validate_returns_errors_for_bad_scenario() {
        let app = build_test_app();

        // Valid scenario should return 200 with valid=true
        let valid_scenario = serde_json::json!({
            "version": 1, "name": "valid-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/validate")
                    .header("content-type", "application/json")
                    .body(Body::from(valid_scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["validation"]["valid"], true);

        // Invalid scenario: version=0, empty name, timeline references unknown actor
        let invalid_scenario = serde_json::json!({
            "version": 0, "name": "", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {},
            "timeline": [
                { "at_ms": 1000, "actor_id": "ghost", "action": { "type": "create_folder", "parent_id": "root", "name": "X" } }
            ],
            "faults": [], "assertions": []
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/scenario/validate")
                    .header("content-type", "application/json")
                    .body(Body::from(invalid_scenario.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = json_body(resp).await;
        assert_eq!(body["validation"]["valid"], false);
        let errors = body["validation"]["errors"].as_array().unwrap();
        assert!(errors.len() >= 2, "should have multiple errors: {errors:?}");
    }

    // ── DR-6: Upload + download content round-trip via V3 ──────────────

    #[tokio::test]
    async fn upload_download_content_round_trip_via_v3() {
        let app = build_test_app();

        // Seed root folder
        let scenario = serde_json::json!({
            "version": 1, "name": "upload-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // Upload a file with raw content
        let file_content = b"Hello, this is file content for testing!";
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?name=test.txt&mimeType=text/plain&parents=root")
                    .header("content-type", "text/plain")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(file_content.as_slice()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["name"], "test.txt");
        assert_eq!(created["mimeType"], "text/plain");
        let file_id = created["id"].as_str().unwrap();

        // Download via alt=media
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}?alt=media"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap().to_str().unwrap(),
            "text/plain"
        );
        let downloaded = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(downloaded.as_ref(), file_content);

        // Get metadata (no alt=media) should return JSON, not content
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        assert_eq!(meta["name"], "test.txt");
        assert_eq!(meta["kind"], "drive#file");
    }

    // ── R1+R2: appProperties and webViewLink via V3 upload ─────────────

    #[tokio::test]
    async fn v3_upload_with_app_properties_and_web_view_link() {
        let app = build_test_app();

        // Seed root
        let scenario = serde_json::json!({
            "version": 1, "name": "app-props-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // Upload a file with appProperties via header
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?name=config.json&mimeType=application/json&parents=root")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("X-Twin-App-Properties", r#"{"role":"config","version":"3"}"#)
                    .body(Body::from(r#"{"key":"value"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        let file_id = created["id"].as_str().unwrap();

        // appProperties should be in the create response
        assert_eq!(created["appProperties"]["role"], "config");
        assert_eq!(created["appProperties"]["version"], "3");

        // webViewLink should be populated
        let expected_link = format!("https://drive.google.com/file/d/{file_id}/view");
        assert_eq!(created["webViewLink"], expected_link);

        // Get file metadata and verify again
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        assert_eq!(meta["appProperties"]["role"], "config");
        assert_eq!(meta["appProperties"]["version"], "3");
        assert_eq!(meta["webViewLink"], expected_link);
    }

    // ── R3: Compound V3 query filtering ────────────────────────────────

    #[tokio::test]
    async fn v3_list_files_compound_query() {
        let app = build_test_app();

        // Seed with diverse files
        let scenario = serde_json::json!({
            "version": 1, "name": "compound-query-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "f1", "name": "report.pdf", "parent_id": "root", "owner_id": "alice", "kind": "File",
                      "mime_type": "application/pdf" },
                    { "id": "f2", "name": "report.pdf", "parent_id": "root", "owner_id": "alice", "kind": "File",
                      "mime_type": "text/plain" },
                    { "id": "f3", "name": "notes.txt", "parent_id": "root", "owner_id": "alice", "kind": "File",
                      "mime_type": "text/plain" },
                    { "id": "f4", "name": "data.json", "parent_id": "root", "owner_id": "alice", "kind": "File",
                      "mime_type": "application/json",
                      "app_properties": { "role": "config" } }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // Query: name = 'report.pdf' and mimeType = 'application/pdf'
        // q='root' in parents and name = 'report.pdf' and mimeType = 'application/pdf'
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files?q=%27root%27%20in%20parents%20and%20name%20%3D%20%27report.pdf%27%20and%20mimeType%20%3D%20%27application%2Fpdf%27")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "should match only the PDF report");
        assert_eq!(files[0]["name"], "report.pdf");
        assert_eq!(files[0]["mimeType"], "application/pdf");

        // Query: appProperties has { key='role' and value='config' }
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files?q=appProperties%20has%20%7B%20key%3D%27role%27%20and%20value%3D%27config%27%20%7D")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "should match only the config file");
        assert_eq!(files[0]["name"], "data.json");

        // Query: just parent, no other filters — should return all 4 files
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files?q=%27root%27%20in%20parents")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), 4, "should return all files under root");
    }

    // ── R1: appProperties via scenario seed ────────────────────────────

    #[tokio::test]
    async fn v3_seed_with_app_properties() {
        let app = build_test_app();

        // Seed with a file that has app_properties
        let scenario = serde_json::json!({
            "version": 1, "name": "seed-app-props", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "f1", "name": "manifest.json", "parent_id": "root", "owner_id": "alice", "kind": "File",
                      "mime_type": "application/json",
                      "app_properties": { "source": "whizy", "env": "test" } }
                ]
            },
            "timeline": [], "faults": [], "assertions": []
        });
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // Get the file via V3 and verify appProperties came through the seed
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/f1")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        assert_eq!(meta["appProperties"]["source"], "whizy");
        assert_eq!(meta["appProperties"]["env"], "test");
        assert!(meta["webViewLink"].as_str().unwrap().contains("/f1/view"));

        // Verify state inspection also shows app_properties
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/state/items/f1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state = json_body(resp).await;
        let item = &state["item"];
        assert_eq!(item["properties"]["app_properties"]["source"], "whizy");
        assert_eq!(item["properties"]["app_properties"]["env"], "test");
    }

    // ----- Discovery document tests -----

    #[tokio::test]
    async fn discovery_v2_endpoint_returns_drive_document() {
        let app = build_test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/$discovery/rest?version=v3")
                    .header("host", "localhost:9100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = json_body(resp).await;
        assert_eq!(doc["kind"], "discovery#restDescription");
        assert_eq!(doc["name"], "drive");
        assert_eq!(doc["version"], "v3");
        assert_eq!(doc["rootUrl"], "http://localhost:9100/");
        assert_eq!(doc["servicePath"], "drive/v3/");
        assert_eq!(
            doc["baseUrl"],
            "http://localhost:9100/drive/v3/"
        );

        // Verify files resource methods exist
        let files = &doc["resources"]["files"];
        assert!(files["methods"]["list"].is_object());
        assert!(files["methods"]["get"].is_object());
        assert!(files["methods"]["create"].is_object());
        assert!(files["methods"]["update"].is_object());
        assert!(files["methods"]["delete"].is_object());

        // Verify nested permissions resource
        let perms = &files["resources"]["permissions"];
        assert!(perms["methods"]["create"].is_object());

        // Verify media upload on create
        assert_eq!(
            doc["resources"]["files"]["methods"]["create"]["supportsMediaUpload"],
            true
        );
    }

    #[tokio::test]
    async fn discovery_v1_endpoint_returns_drive_document() {
        let app = build_test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/discovery/v1/apis/drive/v3/rest")
                    .header("host", "localhost:9100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = json_body(resp).await;
        assert_eq!(doc["name"], "drive");
        assert_eq!(doc["version"], "v3");
    }

    #[tokio::test]
    async fn discovery_does_not_require_auth() {
        // Build a server with reject_unauthenticated=true
        let (runs, scenarios) = create_test_dirs();
        let config = ServerConfig {
            runs_dir: runs,
            scenarios_dir: scenarios,
            twin_config: TwinConfig {
                seed: 42,
                start_time_unix_ms: 1_704_067_200_000,
            },
            auth: twin_server_core::AuthConfig {
                actors: std::collections::HashMap::new(),
                reject_unauthenticated: true,
            },
        };
        let app = build_twin_router::<DriveTwinService>(config);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/$discovery/rest?version=v3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Discovery should work without any auth headers
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = json_body(resp).await;
        assert_eq!(doc["name"], "drive");
    }

    // --- /control/seed E2E tests ---

    #[tokio::test]
    async fn control_seed_creates_files() {
        let app = build_test_app();

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "files": [
                                {
                                    "id": "root",
                                    "name": "My Drive",
                                    "parent_id": null,
                                    "owner_id": "alice",
                                    "kind": "Folder"
                                },
                                {
                                    "id": "f1",
                                    "name": "notes.txt",
                                    "parent_id": "root",
                                    "owner_id": "alice",
                                    "kind": "File",
                                    "mime_type": "text/plain",
                                    "content": "aGVsbG8=",
                                    "app_properties": { "source": "test" }
                                }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "ok");

        // Verify file exists via v3 files.get (as alice, the owner)
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/f1?fields=*")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let file = json_body(resp).await;
        assert_eq!(file["name"], "notes.txt");
        assert_eq!(file["mimeType"], "text/plain");
        assert_eq!(file["appProperties"]["source"], "test");
    }

    #[tokio::test]
    async fn control_seed_resets_existing_state() {
        let app = build_test_app();

        // First seed
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "files": [
                                { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                                { "id": "old_file", "name": "old.txt", "parent_id": "root", "owner_id": "alice", "kind": "File" }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Second seed — different data
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/seed")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "files": [
                                { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "bob", "kind": "Folder" },
                                { "id": "new_file", "name": "new.txt", "parent_id": "root", "owner_id": "bob", "kind": "File" }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // old_file should be gone
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/old_file?fields=*")
                    .header("X-Twin-Actor-Id", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // new_file should exist
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/new_file?fields=*")
                    .header("X-Twin-Actor-Id", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let file = json_body(resp).await;
        assert_eq!(file["name"], "new.txt");
    }

    #[tokio::test]
    async fn control_seed_empty_body() {
        let app = build_test_app();

        let resp = app
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
        assert_eq!(resp.status(), StatusCode::OK);

        // v3 files.list should return nothing (no root, no files)
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let files = list["files"].as_array().unwrap();
        assert!(files.is_empty());
    }

    // --- Multipart upload E2E tests ---

    fn build_multipart_body(
        boundary: &str,
        metadata_json: &str,
        content: &[u8],
        content_type: &str,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
        body.extend_from_slice(metadata_json.as_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(content);
        body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
        body
    }

    #[tokio::test]
    async fn multipart_upload_creates_file() {
        let app = build_test_app();

        // Reset first to ensure clean state
        let _ = app
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

        let boundary = "===============boundary123==";
        let metadata = serde_json::json!({
            "name": "multipart_doc.txt",
            "parents": ["root"],
            "mimeType": "text/plain",
            "appProperties": { "source": "sdk" }
        });
        let content = b"Hello from multipart upload!";
        let body = build_multipart_body(boundary, &metadata.to_string(), content, "text/plain");

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=multipart")
                    .header(
                        "content-type",
                        format!("multipart/related; boundary={boundary}"),
                    )
                    .header("X-Twin-Actor-Id", "system")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let file = json_body(resp).await;
        assert_eq!(file["name"], "multipart_doc.txt");
        assert_eq!(file["mimeType"], "text/plain");
        assert_eq!(file["appProperties"]["source"], "sdk");
        let file_id = file["id"].as_str().unwrap().to_string();

        // Download content to verify
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}?alt=media"))
                    .header("X-Twin-Actor-Id", "system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"Hello from multipart upload!");
    }

    #[tokio::test]
    async fn multipart_upload_metadata_only_name() {
        let app = build_test_app();

        // Reset
        let _ = app
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

        // Minimal metadata: just name (should default parent to root)
        let boundary = "bnd";
        let metadata = serde_json::json!({ "name": "minimal.bin" });
        let content = b"\x00\x01\x02\x03";
        let body =
            build_multipart_body(boundary, &metadata.to_string(), content, "application/octet-stream");

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=multipart")
                    .header("content-type", format!("multipart/related; boundary={boundary}"))
                    .header("X-Twin-Actor-Id", "system")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let file = json_body(resp).await;
        assert_eq!(file["name"], "minimal.bin");
        assert_eq!(file["parents"], serde_json::json!(["root"]));
    }

    #[tokio::test]
    async fn multipart_upload_missing_boundary_returns_400() {
        let app = build_test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=multipart")
                    .header("content-type", "multipart/related")
                    .header("X-Twin-Actor-Id", "system")
                    .body(Body::from("garbage"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
