use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioDocument {
    pub version: u32,
    pub name: String,
    pub seed: u64,
    pub start_time_unix_ms: i64,
    pub actors: Vec<Actor>,
    /// Opaque JSON value — each twin defines its own seed schema.
    /// The generic server core passes this through to `TwinService::seed_from_scenario`
    /// without interpreting it, so twin-specific fields (e.g. `mime_type`, `content`)
    /// are preserved.
    pub initial_state: serde_json::Value,
    pub timeline: Vec<TimelineEvent>,
    pub faults: Vec<FaultRule>,
    pub assertions: Vec<AssertionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub at_ms: u64,
    pub actor_id: String,
    /// Opaque JSON value — each twin defines its own action schema.
    /// The generic server core passes this through to `TwinService::execute_timeline_action`
    /// without interpreting it.
    pub action: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    CreateFile {
        parent_id: String,
        name: String,
    },
    CreateFolder {
        parent_id: String,
        name: String,
    },
    SetPermission {
        item_id: String,
        target_actor_id: String,
        role: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultRule {
    pub id: String,
    pub when: FaultWhen,
    pub effect: FaultEffect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultWhen {
    pub endpoint: String,
    pub actor_id: Option<String>,
    pub probability: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaultEffect {
    HttpError { status: u16, message: String },
    Latency { delay_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionRule {
    pub id: String,
    /// Opaque JSON value — each twin defines its own assertion schema.
    /// The generic server core passes this through to `TwinService::evaluate_assertion`
    /// without interpreting it.
    pub check: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssertionCheck {
    NoOrphans,
    ActorCanAccess { actor_id: String, item_id: String },
    ItemExists { item_id: String },
}

pub fn parse_scenario_json(input: &str) -> Result<ScenarioDocument, serde_json::Error> {
    serde_json::from_str(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_scenario() {
        let raw = r#"
        {
          "version": 1,
          "name": "test",
          "seed": 42,
          "start_time_unix_ms": 1704067200000,
          "actors": [{ "id": "a1", "label": "A1" }],
          "initial_state": { "files": [] },
          "timeline": [],
          "faults": [],
          "assertions": []
        }
        "#;
        let doc = parse_scenario_json(raw).unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.name, "test");
        assert_eq!(doc.actors.len(), 1);
    }

    #[test]
    fn initial_state_preserves_twin_specific_fields() {
        let raw = r#"
        {
          "version": 1,
          "name": "content-test",
          "seed": 42,
          "start_time_unix_ms": 1704067200000,
          "actors": [],
          "initial_state": {
            "files": [{
              "id": "root",
              "name": "My Drive",
              "owner_id": "alice",
              "kind": "Folder"
            }, {
              "id": "f1",
              "name": "readme.md",
              "parent_id": "root",
              "owner_id": "alice",
              "kind": "File",
              "mime_type": "text/markdown",
              "content": "SGVsbG8gV29ybGQ="
            }]
          },
          "timeline": [],
          "faults": [],
          "assertions": []
        }
        "#;
        let doc = parse_scenario_json(raw).unwrap();
        // Verify twin-specific fields survived deserialization
        let files = doc.initial_state["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        let f1 = &files[1];
        assert_eq!(f1["mime_type"].as_str().unwrap(), "text/markdown");
        assert_eq!(f1["content"].as_str().unwrap(), "SGVsbG8gV29ybGQ=");
    }
}
