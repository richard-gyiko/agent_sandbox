use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwinConfig {
    pub seed: u64,
    pub start_time_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwinState {
    pub revision: u64,
    pub metadata: BTreeMap<String, String>,
    pub service_state: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwinEvent {
    pub sequence: u64,
    pub endpoint: String,
    pub actor_id: Option<String>,
    pub outcome: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub fault_id: Option<String>,
    pub logical_time_unix_ms: i64,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TwinEventContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug)]
pub struct TwinKernel {
    config: TwinConfig,
    state: TwinState,
    events: Vec<TwinEvent>,
    next_event_sequence: u64,
    active_session: Option<String>,
}

impl TwinKernel {
    pub fn new(config: TwinConfig) -> Self {
        Self {
            config,
            state: TwinState {
                revision: 0,
                metadata: BTreeMap::new(),
                service_state: serde_json::Value::Null,
            },
            events: Vec::new(),
            next_event_sequence: 1,
            active_session: None,
        }
    }

    pub fn snapshot(&self) -> TwinState {
        self.state.clone()
    }

    pub fn restore(&mut self, state: TwinState) {
        self.state = state;
    }

    pub fn reset(&mut self, config: TwinConfig) {
        self.config = config;
        self.state = TwinState {
            revision: 0,
            metadata: BTreeMap::new(),
            service_state: serde_json::Value::Null,
        };
        self.events.clear();
        self.next_event_sequence = 1;
        // Note: active_session is NOT cleared by reset — session lifecycle
        // is managed explicitly via set_active_session/clear_active_session.
    }

    pub fn set_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.state.revision += 1;
        self.state.metadata.insert(key.into(), value.into());
    }

    pub fn config(&self) -> &TwinConfig {
        &self.config
    }

    pub fn record_event(
        &mut self,
        endpoint: impl Into<String>,
        actor_id: Option<String>,
        outcome: impl Into<String>,
        detail: impl Into<String>,
        fault_id: Option<String>,
    ) {
        self.record_event_with_context(
            endpoint,
            actor_id,
            outcome,
            detail,
            fault_id,
            TwinEventContext::default(),
        );
    }

    pub fn record_event_with_context(
        &mut self,
        endpoint: impl Into<String>,
        actor_id: Option<String>,
        outcome: impl Into<String>,
        detail: impl Into<String>,
        fault_id: Option<String>,
        context: TwinEventContext,
    ) {
        let event = TwinEvent {
            sequence: self.next_event_sequence,
            endpoint: endpoint.into(),
            actor_id,
            outcome: outcome.into(),
            detail: detail.into(),
            operation: context.operation,
            resource: context.resource,
            request_id: context.request_id,
            trace_id: context.trace_id,
            fault_id,
            logical_time_unix_ms: self.config.start_time_unix_ms + self.state.revision as i64,
            session_id: self.active_session.clone(),
        };
        self.next_event_sequence += 1;
        self.events.push(event);
    }

    pub fn events(&self) -> &[TwinEvent] {
        &self.events
    }

    /// Set the active session ID. All subsequent events will be tagged with this session.
    pub fn set_active_session(&mut self, session_id: String) {
        self.active_session = Some(session_id);
    }

    /// Clear the active session. Subsequent events will have no session tag.
    pub fn clear_active_session(&mut self) {
        self.active_session = None;
    }

    /// Returns the currently active session ID, if any.
    pub fn active_session(&self) -> Option<&str> {
        self.active_session.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_state_round_trips_through_serde() {
        let state = TwinState {
            revision: 5,
            metadata: BTreeMap::new(),
            service_state: serde_json::json!({"files": [1, 2, 3]}),
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: TwinState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.service_state, state.service_state);
    }

    #[test]
    fn event_session_id_round_trips_through_serde() {
        let event = TwinEvent {
            sequence: 1,
            endpoint: "/test".to_string(),
            actor_id: None,
            outcome: "ok".to_string(),
            detail: "test".to_string(),
            operation: Some("GET".to_string()),
            resource: Some("/test".to_string()),
            request_id: Some("req_1".to_string()),
            trace_id: Some("trace_1".to_string()),
            fault_id: None,
            logical_time_unix_ms: 1000,
            session_id: Some("sess_1".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: TwinEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_id, Some("sess_1".to_string()));
        assert_eq!(restored.request_id, Some("req_1".to_string()));
    }

    #[test]
    fn event_round_trip_defaults_new_optional_fields() {
        // Simulate old persisted payloads that don't include new fields.
        let raw = serde_json::json!({
            "sequence": 1,
            "endpoint": "/test",
            "actor_id": null,
            "outcome": "ok",
            "detail": "test",
            "fault_id": null,
            "logical_time_unix_ms": 1000,
            "session_id": null
        });
        let restored: TwinEvent = serde_json::from_value(raw).unwrap();
        assert!(restored.operation.is_none());
        assert!(restored.resource.is_none());
        assert!(restored.request_id.is_none());
        assert!(restored.trace_id.is_none());
    }

    #[test]
    fn record_event_tags_with_active_session() {
        let mut kernel = TwinKernel::new(TwinConfig {
            seed: 42,
            start_time_unix_ms: 1000,
        });

        // No session — event has no session_id
        kernel.record_event("/a", None, "ok", "no session", None);
        assert_eq!(kernel.events()[0].session_id, None);

        // Set session — event gets tagged
        kernel.set_active_session("sess_1".to_string());
        kernel.record_event("/b", None, "ok", "with session", None);
        assert_eq!(kernel.events()[1].session_id, Some("sess_1".to_string()));

        // Clear session — event has no session_id again
        kernel.clear_active_session();
        kernel.record_event("/c", None, "ok", "after clear", None);
        assert_eq!(kernel.events()[2].session_id, None);
    }

    #[test]
    fn active_session_returns_current_session() {
        let mut kernel = TwinKernel::new(TwinConfig {
            seed: 42,
            start_time_unix_ms: 1000,
        });
        assert_eq!(kernel.active_session(), None);
        kernel.set_active_session("s1".to_string());
        assert_eq!(kernel.active_session(), Some("s1"));
        kernel.clear_active_session();
        assert_eq!(kernel.active_session(), None);
    }

    #[test]
    fn reset_preserves_active_session() {
        let mut kernel = TwinKernel::new(TwinConfig {
            seed: 42,
            start_time_unix_ms: 1000,
        });
        kernel.set_active_session("s1".to_string());
        kernel.reset(TwinConfig {
            seed: 1,
            start_time_unix_ms: 0,
        });
        // reset does NOT clear the active session — session lifecycle is explicit
        assert_eq!(kernel.active_session(), Some("s1"));
    }
}
