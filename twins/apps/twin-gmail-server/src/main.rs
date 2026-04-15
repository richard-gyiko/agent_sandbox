//! Thin binary for the gmail digital twin.
//!
//! Wires [`GmailTwinService`] into the generic twin-server-core host and
//! starts listening.  All control-surface logic (scenarios, runs, fault
//! injection, etc.) is provided by `twin-server-core`.

use std::net::SocketAddr;
use tracing::info;
use twin_gmail::GmailTwinService;
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
    let app = build_twin_router::<GmailTwinService>(env_config.server);

    let addr = SocketAddr::from((host, port));
    info!("twin-gmail-server listening on {}", addr);

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
            std::env::temp_dir().join(format!("twin-gmail-server-tests-{pid}-{nanos}-{id}"));
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
        build_twin_router::<GmailTwinService>(config)
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
        build_twin_router::<GmailTwinService>(config)
    }

    /// Helper: read response body as JSON.
    async fn json_body(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── Basic health & control routes ──────────────────────────────────

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
    async fn control_routes_are_registered() {
        let app = build_test_app();

        // Snapshot
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

        // Events
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

    // ── Native route smoke test ────────────────────────────────────────

    #[tokio::test]
    async fn native_gmail_routes_are_registered() {
        let app = build_test_app();

        // Reset first
        let _reset = app
            .clone()
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

        // Send a message via native route
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/messages/send")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "to": ["bob@example.com"],
                            "subject": "Hello",
                            "body": "Hi Bob"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // List labels via native route
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/labels")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Scenario apply with Gmail operations ───────────────────────────

    #[tokio::test]
    async fn scenario_apply_with_gmail_operations() {
        let app = build_test_app();

        let scenario = serde_json::json!({
            "version": 1,
            "name": "gmail-e2e",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [
                { "id": "alice", "label": "Alice" },
                { "id": "bob",   "label": "Bob"   }
            ],
            "initial_state": {
                "messages": [
                    {
                        "id": "msg_seed_1",
                        "from": "bob@example.com",
                        "to": ["alice@example.com"],
                        "subject": "Welcome",
                        "body": "Welcome to Gmail twin!",
                        "label_ids": ["INBOX", "UNREAD"]
                    }
                ],
                "labels": [
                    {
                        "id": "Label_custom",
                        "name": "My Custom Label",
                        "label_type": "user"
                    }
                ]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": {
                        "type": "send_message",
                        "to": ["bob@example.com"],
                        "subject": "Re: Welcome",
                        "body": "Thanks Bob!"
                    }
                }
            ],
            "faults": [],
            "assertions": [
                {
                    "id": "seed-msg-exists",
                    "check": { "type": "message_exists", "message_id": "msg_seed_1" }
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

        let report = json_body(response).await;
        assert_eq!(report["status"], "ok", "scenario should pass: {report}");
    }

    // ── Full E2E SDK mimicry with auth ─────────────────────────────────

    #[tokio::test]
    async fn e2e_sdk_mimicry_with_auth() {
        // ── 1. Setup: create app with auth config ──
        let mut actors = std::collections::HashMap::new();
        actors.insert("token_alice".to_string(), "alice".to_string());
        actors.insert("token_bob".to_string(), "bob".to_string());
        let app = build_test_app_with_auth(actors);

        // ── 2. Seed initial state via scenario/apply ──
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
                "messages": [
                    {
                        "id": "msg_1",
                        "from": "external@example.com",
                        "to": ["alice@example.com"],
                        "subject": "Important meeting",
                        "body": "Meeting at 3pm tomorrow",
                        "label_ids": ["INBOX", "UNREAD"]
                    }
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

        // ── 3. Get profile ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/profile")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let profile = json_body(resp).await;
        assert_eq!(profile["emailAddress"], "alice@twin.local");

        // ── 4. List messages ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let messages = list["messages"].as_array().unwrap();
        assert!(
            !messages.is_empty(),
            "should have at least the seeded message"
        );
        let seeded_msg_id = messages
            .iter()
            .find(|m| m["id"] == "msg_1")
            .expect("seeded message should be in list")["id"]
            .as_str()
            .unwrap()
            .to_string();

        // ── 5. Get seeded message (full format) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/{seeded_msg_id}?format=full"
                    ))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let msg = json_body(resp).await;
        assert_eq!(msg["id"], seeded_msg_id);
        assert!(
            msg["payload"]["headers"].is_array(),
            "full format should include headers"
        );
        assert!(
            msg["payload"]["parts"].is_array() || msg["payload"]["body"].is_object(),
            "full format should include body/parts"
        );

        // ── 6. Send a message as Alice via v1 ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/v1/users/me/messages/send")
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({
                            "to": ["bob@example.com"],
                            "subject": "Test from Alice",
                            "body": "Hello Bob, this is a test."
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "send should succeed");
        let sent = json_body(resp).await;
        let sent_msg_id = sent["id"].as_str().unwrap().to_string();
        assert!(!sent_msg_id.is_empty());
        // Sent messages should have SENT label
        let sent_labels = sent["labelIds"].as_array().unwrap();
        assert!(
            sent_labels.iter().any(|l| l == "SENT"),
            "sent message should have SENT label, got: {sent_labels:?}"
        );

        // ── 7. Get the sent message's thread ──
        let sent_thread_id = sent["threadId"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!(
                        "/gmail/v1/users/me/threads/{sent_thread_id}"
                    ))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let thread = json_body(resp).await;
        assert_eq!(thread["id"], sent_thread_id);
        let thread_messages = thread["messages"].as_array().unwrap();
        assert!(
            !thread_messages.is_empty(),
            "thread should have at least one message"
        );

        // ── 8. List threads ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/threads")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let threads_list = json_body(resp).await;
        let threads = threads_list["threads"].as_array().unwrap();
        assert!(
            !threads.is_empty(),
            "should have at least one thread"
        );

        // ── 9. Modify message labels (add STARRED, remove UNREAD) ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/{seeded_msg_id}/modify"
                    ))
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({
                            "addLabelIds": ["STARRED"],
                            "removeLabelIds": ["UNREAD"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let modified = json_body(resp).await;
        let mod_labels = modified["labelIds"].as_array().unwrap();
        assert!(
            mod_labels.iter().any(|l| l == "STARRED"),
            "should have STARRED after modify"
        );
        assert!(
            !mod_labels.iter().any(|l| l == "UNREAD"),
            "should not have UNREAD after modify"
        );

        // ── 10. Trash message ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/{seeded_msg_id}/trash"
                    ))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let trashed = json_body(resp).await;
        let trash_labels = trashed["labelIds"].as_array().unwrap();
        assert!(
            trash_labels.iter().any(|l| l == "TRASH"),
            "trashed message should have TRASH label"
        );

        // ── 11. Untrash message ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/{seeded_msg_id}/untrash"
                    ))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let untrashed = json_body(resp).await;
        let untrash_labels = untrashed["labelIds"].as_array().unwrap();
        assert!(
            !untrash_labels.iter().any(|l| l == "TRASH"),
            "untrashed message should not have TRASH label"
        );

        // ── 12. Labels CRUD via v1 ──
        // List labels
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/labels")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let labels_resp = json_body(resp).await;
        let labels = labels_resp["labels"].as_array().unwrap();
        assert!(
            labels.len() >= 13,
            "should have at least 13 system labels, got {}",
            labels.len()
        );

        // Create label
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/v1/users/me/labels")
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({ "name": "E2E Test Label" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let created_label = json_body(resp).await;
        let label_id = created_label["id"].as_str().unwrap().to_string();
        assert_eq!(created_label["name"], "E2E Test Label");
        assert_eq!(created_label["type"], "user");

        // Get label
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/gmail/v1/users/me/labels/{label_id}"))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got_label = json_body(resp).await;
        assert_eq!(got_label["id"], label_id);
        assert_eq!(got_label["name"], "E2E Test Label");

        // Update label (PUT)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(&format!("/gmail/v1/users/me/labels/{label_id}"))
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::from(
                        serde_json::json!({ "name": "Renamed Label" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let updated_label = json_body(resp).await;
        assert_eq!(updated_label["name"], "Renamed Label");

        // Delete label
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(&format!("/gmail/v1/users/me/labels/{label_id}"))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify deletion — GET should return 404
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/gmail/v1/users/me/labels/{label_id}"))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // ── 13. Delete message permanently ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/{sent_msg_id}"
                    ))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "delete should return 204");

        // Verify deletion — GET should return 404
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/{sent_msg_id}"
                    ))
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "deleted message should return 404");

        // ── 14. Auth: X-Twin-Actor-Id header override ──
        // X-Twin-Actor-Id should work without Bearer token
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/profile")
                    .header("X-Twin-Actor-Id", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bob_profile = json_body(resp).await;
        assert_eq!(
            bob_profile["emailAddress"], "bob@twin.local",
            "X-Twin-Actor-Id should resolve to bob"
        );

        // X-Twin-Actor-Id should take priority over Bearer token
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/profile")
                    .header("X-Twin-Actor-Id", "bob")
                    .header("Authorization", "Bearer token_alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let override_profile = json_body(resp).await;
        assert_eq!(
            override_profile["emailAddress"], "bob@twin.local",
            "X-Twin-Actor-Id should take priority over Bearer token"
        );
    }

    // ── Thread operations via v1 ───────────────────────────────────────

    #[tokio::test]
    async fn e2e_thread_operations() {
        let app = build_test_app();

        // Reset
        let _reset = app
            .clone()
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

        // Send two messages with same subject to create a thread
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/v1/users/me/messages/send")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "to": ["bob@example.com"],
                            "subject": "Thread test",
                            "body": "First message"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let first = json_body(resp).await;
        let thread_id = first["threadId"].as_str().unwrap().to_string();

        // Send reply in same thread
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/v1/users/me/messages/send")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "to": ["bob@example.com"],
                            "subject": "Thread test",
                            "body": "Reply in thread",
                            "threadId": thread_id
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let reply = json_body(resp).await;
        assert_eq!(
            reply["threadId"].as_str().unwrap(),
            thread_id,
            "reply should be in the same thread"
        );

        // Get thread — should have 2 messages
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/gmail/v1/users/me/threads/{thread_id}"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let thread = json_body(resp).await;
        let thread_msgs = thread["messages"].as_array().unwrap();
        assert_eq!(
            thread_msgs.len(),
            2,
            "thread should have 2 messages"
        );

        // Trash thread
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!(
                        "/gmail/v1/users/me/threads/{thread_id}/trash"
                    ))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Untrash thread
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&format!(
                        "/gmail/v1/users/me/threads/{thread_id}/untrash"
                    ))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Delete thread permanently
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(&format!(
                        "/gmail/v1/users/me/threads/{thread_id}"
                    ))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify deletion
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!(
                        "/gmail/v1/users/me/threads/{thread_id}"
                    ))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── Agent session workflow ──────────────────────────────────────────

    #[tokio::test]
    async fn e2e_agent_session_workflow() {
        let app = build_test_app();

        // ── Step 1: Create session ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "gmail-e2e-session",
                            "seed": {
                                "messages": [
                                    {
                                        "id": "seed_1",
                                        "from": "external@example.com",
                                        "to": ["alice@example.com"],
                                        "subject": "Session test",
                                        "body": "Session test body",
                                        "label_ids": ["INBOX", "UNREAD"]
                                    }
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
        assert!(!session_id.is_empty());

        // ── Step 2: Apply scenario ──
        let scenario = serde_json::json!({
            "version": 1,
            "name": "gmail-session-scenario",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "messages": [
                    {
                        "id": "msg_s1",
                        "from": "bob@example.com",
                        "to": ["alice@example.com"],
                        "subject": "Scenario message",
                        "body": "Body here",
                        "label_ids": ["INBOX"]
                    }
                ]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": {
                        "type": "send_message",
                        "to": ["bob@example.com"],
                        "subject": "Reply",
                        "body": "Got it"
                    }
                }
            ],
            "faults": [],
            "assertions": [
                {
                    "id": "msg-exists",
                    "check": { "type": "message_exists", "message_id": "msg_s1" }
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

        // ── Step 3: Call Gmail v1 APIs ──
        // List messages
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let messages = list["messages"].as_array().unwrap();
        assert!(
            messages.len() >= 2,
            "should have at least 2 messages (seeded + timeline), got {}",
            messages.len()
        );

        // Send via v1
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/v1/users/me/messages/send")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "to": ["charlie@example.com"],
                            "subject": "Session API test",
                            "body": "Testing from session"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // ── Step 4: Inspect state ──
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
        assert!(
            !items.is_empty(),
            "state inspection should return items"
        );

        // ── Step 5: End session ──
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

        // ── Step 6: Query events by session ──
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/control/events?session_id={session_id}"))
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

        // ── Step 7: Get session metadata ──
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
        assert_eq!(meta["name"], "gmail-e2e-session");

        // ── Step 8: List sessions ──
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

        // ── Step 9: Snapshot ──
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
        assert!(snapshot.is_object(), "snapshot should be a JSON object");
    }

    // ── Message format variants ────────────────────────────────────────

    #[tokio::test]
    async fn e2e_message_format_variants() {
        let app = build_test_app();

        // Seed a message
        let scenario = serde_json::json!({
            "version": 1,
            "name": "format-test",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "messages": [{
                    "id": "fmt_msg",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "Format test",
                    "body": "Plain text body",
                    "body_html": "<p>HTML body</p>",
                    "label_ids": ["INBOX"]
                }]
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
        assert_eq!(resp.status(), StatusCode::OK);

        // Get message in metadata format — should have headers but no body parts
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/fmt_msg?format=metadata")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let metadata_msg = json_body(resp).await;
        assert_eq!(metadata_msg["id"], "fmt_msg");
        assert!(
            metadata_msg["payload"]["headers"].is_array(),
            "metadata format should include headers"
        );

        // Get message in minimal format — should have id and labels but no payload
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/fmt_msg?format=minimal")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let minimal_msg = json_body(resp).await;
        assert_eq!(minimal_msg["id"], "fmt_msg");
        // Minimal should NOT have payload (or it should be null/absent)
        assert!(
            minimal_msg.get("payload").is_none() || minimal_msg["payload"].is_null(),
            "minimal format should not include payload"
        );

        // Get in full format — should have parts with body
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/fmt_msg?format=full")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let full_msg = json_body(resp).await;
        assert_eq!(full_msg["id"], "fmt_msg");
        assert!(
            full_msg["payload"].is_object(),
            "full format should include payload"
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
        build_twin_router::<GmailTwinService>(config)
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
                    .uri("/gmail/v1/users/me/messages")
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
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Authentication required"),
            "401 body should explain that auth is needed: {body}"
        );
    }

    #[tokio::test]
    async fn reject_unauthenticated_allows_bearer_token() {
        let app = build_test_app_reject_unauth();

        // Seed state — auth middleware applies to ALL routes including control
        let scenario = serde_json::json!({
            "version": 1, "name": "auth-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "messages": [{
                    "id": "auth_msg",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "Auth test",
                    "body": "Hello",
                    "label_ids": ["INBOX"]
                }]
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
                    .uri("/gmail/v1/users/me/messages")
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
                    .uri("/gmail/v1/users/me/messages")
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
        let app = build_test_app();

        // Seed state
        let scenario = serde_json::json!({
            "version": 1, "name": "error-shape-test", "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "messages": [{
                    "id": "err_msg",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "Error test",
                    "body": "Hello",
                    "label_ids": ["INBOX"]
                }]
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

        // 404: GET a non-existent message
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/does-not-exist")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json_body(resp).await;
        assert_eq!(
            body["error"]["code"], 404,
            "error.code should be 404: {body}"
        );
        assert!(
            body["error"]["message"].as_str().is_some(),
            "error.message should be a string: {body}"
        );
    }

    // ── GM-1: Attachment round-trip via HTTP ────────────────────────────

    #[tokio::test]
    async fn attachment_round_trip_via_v1_http() {
        let app = build_test_app();

        // Seed a message with an attachment
        // NOTE: SeedAttachment uses "content" (base64), not "data_base64"
        let scenario = serde_json::json!({
            "version": 1,
            "name": "attachment-test",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "messages": [{
                    "id": "msg_att",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "With attachment",
                    "body": "See attached",
                    "label_ids": ["INBOX"],
                    "attachments": [
                        {
                            "filename": "report.pdf",
                            "mime_type": "application/pdf",
                            "content": "SGVsbG8gV29ybGQ="
                        }
                    ]
                }]
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

        // Get the message to find the attachment ID
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/msg_att?format=full")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let msg_status = resp.status();
        let msg = json_body(resp).await;
        assert_eq!(msg_status, StatusCode::OK, "GET message returned {msg_status}: {msg}");

        // Find the attachment part — look in payload.parts for a part with body.attachmentId
        let parts = msg["payload"]["parts"]
            .as_array()
            .expect("full format message should have parts");
        let attachment_part = parts
            .iter()
            .find(|p| p["body"]["attachmentId"].as_str().is_some())
            .expect("should have a part with attachmentId");
        let attachment_id = attachment_part["body"]["attachmentId"].as_str().unwrap();
        assert_eq!(attachment_part["filename"], "report.pdf");

        // Download the attachment via the v1 attachment endpoint
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!(
                        "/gmail/v1/users/me/messages/msg_att/attachments/{attachment_id}"
                    ))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let att = json_body(resp).await;
        assert!(
            att["data"].as_str().is_some(),
            "attachment response should have base64url data: {att}"
        );
        assert!(
            att["size"].as_u64().unwrap() > 0,
            "attachment should have non-zero size"
        );
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
                "messages": [{
                    "id": "msg_snap",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "Snapshot test",
                    "body": "Will this survive?",
                    "label_ids": ["INBOX"]
                }]
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
        assert!(
            snapshot["service_state"].to_string().contains("Snapshot test"),
            "snapshot should contain our message"
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

        // Verify reset cleared state — message should be gone
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/msg_snap")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

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

        // Verify state is restored — message should be back
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/msg_snap?format=full")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let msg = json_body(resp).await;
        assert_eq!(msg["id"], "msg_snap");
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
                "messages": [{
                    "id": "v_msg",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "Valid",
                    "body": "Hi",
                    "label_ids": ["INBOX"]
                }]
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
                { "at_ms": 1000, "actor_id": "ghost", "action": { "type": "send_message", "to": ["x@x.com"], "subject": "X", "body": "X" } }
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

    // ── CS-6: Scenario replay + determinism verification ───────────────

    #[tokio::test]
    async fn scenario_replay_determinism_verification() {
        let app = build_test_app();

        // Apply a scenario with timeline actions
        let scenario = serde_json::json!({
            "version": 1,
            "name": "determinism-gmail",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "messages": [{
                    "id": "seed_msg",
                    "from": "bob@example.com",
                    "to": ["alice@example.com"],
                    "subject": "Seeded",
                    "body": "Hello",
                    "label_ids": ["INBOX"]
                }]
            },
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": { "type": "send_message", "to": ["carol@example.com"], "subject": "Reply", "body": "Hi Carol" }
                },
                {
                    "at_ms": 2000,
                    "actor_id": "alice",
                    "action": { "type": "create_label", "name": "Important" }
                }
            ],
            "faults": [],
            "assertions": [
                { "id": "seed-exists", "check": { "type": "message_exists", "message_id": "seed_msg" } }
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

    // ── FI-2: Fault injection E2E with real Gmail twin ─────────────────

    #[tokio::test]
    async fn fault_injection_http_error_with_real_twin() {
        let app = build_test_app();

        let scenario = serde_json::json!({
            "version": 1,
            "name": "fault-gmail-e2e",
            "seed": 42,
            "start_time_unix_ms": 1_704_067_200_000i64,
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {},
            "timeline": [
                {
                    "at_ms": 1000,
                    "actor_id": "alice",
                    "action": { "type": "send_message", "to": ["bob@example.com"], "subject": "Blocked", "body": "This should fail" }
                }
            ],
            "faults": [
                {
                    "id": "force-503",
                    "when": { "endpoint": "send_message", "actor_id": "alice", "probability": 1.0 },
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

    // ── EM-7: Insert message via V1 API ────────────────────────────────

    #[tokio::test]
    async fn insert_message_via_v1_api() {
        let app = build_test_app();

        // Reset state
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control/reset")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "seed": 42, "start_time_unix_ms": 1_704_067_200_000i64 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Insert a message via POST /gmail/v1/users/me/messages
        let insert_body = serde_json::json!({
            "from": "external@example.com",
            "to": ["alice@example.com"],
            "subject": "Imported message",
            "body": "This was imported via insert API",
            "label_ids": ["INBOX", "UNREAD"]
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gmail/v1/users/me/messages")
                    .header("content-type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(insert_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let inserted = json_body(resp).await;
        let msg_id = inserted["id"].as_str().unwrap();
        assert!(!msg_id.is_empty(), "inserted message should have an ID");
        assert!(
            inserted["labelIds"].as_array().is_some(),
            "response should include labelIds"
        );

        // Verify the message is retrievable
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/gmail/v1/users/me/messages/{msg_id}?format=full"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let fetched = json_body(resp).await;
        assert_eq!(fetched["id"], msg_id);

        // Verify it appears in message list
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let messages = list["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|m| m["id"] == msg_id),
            "inserted message should appear in list: {list}"
        );
    }

    // ----- Discovery document tests -----

    #[tokio::test]
    async fn discovery_v2_endpoint_returns_gmail_document() {
        let app = build_test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/$discovery/rest?version=v1")
                    .header("host", "localhost:9200")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = json_body(resp).await;
        assert_eq!(doc["kind"], "discovery#restDescription");
        assert_eq!(doc["name"], "gmail");
        assert_eq!(doc["version"], "v1");
        assert_eq!(doc["rootUrl"], "http://localhost:9200/");
        assert_eq!(doc["servicePath"], "gmail/v1/");

        // Verify users resource with nested sub-resources
        let users = &doc["resources"]["users"];
        assert!(users.is_object(), "users resource missing");

        // users.getProfile direct method
        assert!(users["methods"]["getProfile"].is_object());

        // Nested sub-resources: messages, threads, labels
        let messages = &users["resources"]["messages"];
        assert!(messages["methods"]["list"].is_object());
        assert!(messages["methods"]["get"].is_object());
        assert!(messages["methods"]["send"].is_object());
        assert!(messages["methods"]["insert"].is_object());
        assert!(messages["methods"]["delete"].is_object());
        assert!(messages["methods"]["modify"].is_object());
        assert!(messages["methods"]["trash"].is_object());
        assert!(messages["methods"]["untrash"].is_object());

        // Nested attachments under messages
        let attachments = &messages["resources"]["attachments"];
        assert!(attachments["methods"]["get"].is_object());

        let threads = &users["resources"]["threads"];
        assert!(threads["methods"]["list"].is_object());
        assert!(threads["methods"]["get"].is_object());
        assert!(threads["methods"]["delete"].is_object());
        assert!(threads["methods"]["modify"].is_object());
        assert!(threads["methods"]["trash"].is_object());
        assert!(threads["methods"]["untrash"].is_object());

        let labels = &users["resources"]["labels"];
        assert!(labels["methods"]["list"].is_object());
        assert!(labels["methods"]["get"].is_object());
        assert!(labels["methods"]["create"].is_object());
        assert!(labels["methods"]["update"].is_object());
        assert!(labels["methods"]["patch"].is_object());
        assert!(labels["methods"]["delete"].is_object());
    }

    #[tokio::test]
    async fn discovery_v1_endpoint_returns_gmail_document() {
        let app = build_test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/discovery/v1/apis/gmail/v1/rest")
                    .header("host", "localhost:9200")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = json_body(resp).await;
        assert_eq!(doc["name"], "gmail");
        assert_eq!(doc["version"], "v1");
    }

    #[tokio::test]
    async fn discovery_does_not_require_auth() {
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
        let app = build_twin_router::<GmailTwinService>(config);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/$discovery/rest?version=v1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = json_body(resp).await;
        assert_eq!(doc["name"], "gmail");
    }

    // --- /control/seed E2E tests ---

    #[tokio::test]
    async fn control_seed_creates_messages() {
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
                            "labels": [
                                { "id": "Label_1", "name": "Work", "label_type": "user" }
                            ],
                            "messages": [
                                {
                                    "id": "msg_1",
                                    "thread_id": "thread_1",
                                    "from": "alice@example.com",
                                    "to": ["bob@example.com"],
                                    "subject": "Hello from seed",
                                    "body": "This is seeded content.",
                                    "label_ids": ["INBOX", "Label_1"]
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

        // Verify message exists via v1 messages.get
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/msg_1?format=full")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let msg = json_body(resp).await;
        assert_eq!(msg["id"], "msg_1");
        assert_eq!(msg["threadId"], "thread_1");

        // Verify label exists via v1 labels.get
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/labels/Label_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let label = json_body(resp).await;
        assert_eq!(label["name"], "Work");
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
                            "messages": [{
                                "id": "old_msg",
                                "thread_id": "t1",
                                "from": "alice@example.com",
                                "to": ["bob@example.com"],
                                "subject": "Old message",
                                "label_ids": ["INBOX"]
                            }]
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
                            "messages": [{
                                "id": "new_msg",
                                "thread_id": "t2",
                                "from": "carol@example.com",
                                "to": ["dave@example.com"],
                                "subject": "New message",
                                "label_ids": ["INBOX"]
                            }]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // old_msg should be gone
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/old_msg?format=full")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // new_msg should exist
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages/new_msg?format=full")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let msg = json_body(resp).await;
        assert_eq!(msg["id"], "new_msg");
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

        // v1 messages.list should return nothing
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/gmail/v1/users/me/messages")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        // Empty seed should have no messages (resultSizeEstimate may be 0 or messages may be absent)
        let messages = list.get("messages").and_then(|m| m.as_array());
        assert!(
            messages.is_none() || messages.unwrap().is_empty(),
            "expected no messages after empty seed"
        );
    }
}
