# Architecture

## Overview

A digital twin is a stateful, in-memory replica of a SaaS service. Agents interact with it through the same REST API they would use in production, but every state mutation is recorded as a deterministic event. This gives you snapshot/restore, replay, fault injection, and scripted scenarios — without touching the real service.

The codebase is a Rust workspace organized into layered crates. Each layer has a single responsibility, and dependencies flow strictly downward.

## Crate Dependency Diagram

```
┌─────────────────┐
│   twin-kernel   │  Pure state machine — no external deps beyond serde
└────────┬────────┘
         │
┌────────▼────────┐
│  twin-service   │  TwinService trait, TwinRuntime<T>, SharedTwinState
└────────┬────────┘
         │
┌────────▼────────┐
│ twin-scenario   │  Scenario DSL schema (ScenarioDocument, FaultRule, etc.)
└────────┬────────┘
         │
┌────────▼──────────────┐
│   twin-server-core    │  Generic HTTP host: control routes, auth, scenario engine
└────────┬──────────────┘
         │
    ┌────▼─────┐      ┌──────────────┐
    │twin-drive│      │ twin-<other> │   Twin implementations
    └────┬─────┘      └──────┬───────┘
         │                   │
┌────────▼────────┐  ┌──────▼───────────────┐
│twin-drive-server│  │ twin-<other>-server  │   Thin binary hosts
└─────────────────┘  └─────────────────────-┘

Also in the workspace:
  twin-cli           Scaffolding tool (twin-cli new <name>)
```

## Crate Responsibilities

### twin-kernel

Pure deterministic state engine. Defines `TwinConfig`, `TwinState`, `TwinEvent`, and `TwinKernel`. Handles event append, snapshot export/restore, metadata tracking (created/modified timestamps, version counters), and session tagging. No networking, no async — just data and transitions. This is the foundation that makes replay deterministic.

### twin-service

Defines the `TwinService` trait — the contract every twin implementation must fulfill. Also provides `TwinRuntime<T>` (wraps a `TwinService` with shared state management), `ResolvedActorId`, and the `SharedTwinState` type alias (`Arc<Mutex<TwinRuntime<T>>>`).

### twin-scenario

Schema types for the scenario DSL: `ScenarioDocument`, `ScenarioSeed`, `TimelineAction`, `FaultRule`, `FaultEffect`, `Assertion`, and related enums. Serializes to/from YAML. No execution logic — that lives in `twin-server-core`.

### twin-server-core

The generic HTTP host. Provides `build_twin_router<T: TwinService>()` which assembles the full Axum router for any twin. Contains:

- **Control routes**: reset, snapshot/restore, event log, session management
- **Auth middleware**: token resolution via `actors.json`, `X-Twin-Actor-Id` override, hash fallback
- **Scenario engine**: `run_scenario()`, `validate_scenario()`, `evaluate_assertions()`, fault injection during execution
- **Run persistence**: stores run artifacts to disk, supports listing/retrieval across restarts
- **Session store**: create/end sessions, per-session event streams and snapshots
- **Configuration**: `EnvConfig` (port, dirs, log level), `ServerConfig`, `AuthConfig`

### twin-drive

Google Drive twin implementation. Contains:

- **Domain model**: `DriveItem` (files, folders, shortcuts), `Permission`, `DriveRequest`/`DriveResponse`
- **File content store**: In-memory `BTreeMap<ItemId, Vec<u8>>` blob storage. Files can have binary content uploaded via the upload endpoint or seeded via scenarios. Content is included in snapshots (base64-encoded in JSON) and cleaned up on delete (cascade-delete removes content for all descendants).
- **TwinService impl**: `DriveTwinService` — processes drive commands, produces events
- **Twin-native routes**: `/drive/folders`, `/drive/files`, `/drive/items/*` — explicit, simple API
- **V3 mimicry routes**: `/drive/v3/files`, `/drive/v3/files/{id}`, `/drive/v3/files/{id}/permissions` — matches Google Drive API v3 contract so SDK clients work unmodified
- **Upload route**: `POST /upload/drive/v3/files?uploadType=media` — accepts raw bytes, creates file with content. Supports `name`, `mimeType`, and `parents` query params.
- **Download**: `GET /drive/v3/files/{id}?alt=media` — returns raw bytes with `Content-Type` and `Content-Length` headers. Without `alt=media`, returns JSON metadata.
- **State inspection**: `/state/items`, `/state/tree` — expose internal state for testing. Includes `mime_type`, `size`, and `has_content` fields.

### twin-drive-server (app)

Thin binary. Reads `EnvConfig`, creates a `DriveTwinService`, passes it to `build_twin_router()`, starts the Axum server. Under 40 lines of code.

### twin-cli (app)

Scaffolding tool. `twin-cli new <name>` generates a new twin crate, server binary, Dockerfile, and scenarios directory from templates in `templates/`. Updates the workspace `Cargo.toml` automatically. Templates use `{{name}}` and `{{Name}}` (PascalCase) placeholders.

## Request Flow

```
Agent (HTTP client)
  │
  ▼
Auth middleware
  │  Resolves actor ID from:
  │    1. X-Twin-Actor-Id header
  │    2. Bearer token → actors.json lookup
  │    3. Unknown token → deterministic hash (actor_<hex>)
  │    4. No auth → "default"
  │
  ├──► V3 mimicry route (/drive/v3/files)
  │       │  Translates Google API JSON → internal DriveRequest
  │       ▼
  ├──► Twin-native route (/drive/folders, /drive/items/*)
  │       │
  │       ▼
  │    TwinService::handle_request(request, actor_id)
  │       │
  │       ▼
  │    TwinKernel::append_event(event)
  │       │  Records event with timestamp, sequence number, session tag
  │       ▼
  │    Updated TwinState
  │
  └──► Control route (/control/*)
          │  Operates directly on kernel: reset, snapshot, replay, etc.
          ▼
       TwinKernel
```

## TwinService Trait

The core abstraction that every twin must implement:

```rust
pub trait TwinService: Send + Sync + 'static {
    type Request: DeserializeOwned + Send;
    type Response: Serialize + Send;

    fn handle_request(
        state: &mut TwinState,
        request: Self::Request,
        actor_id: &ResolvedActorId,
    ) -> Result<Self::Response, TwinError>;

    fn router(runtime: TwinRuntime<Self>) -> axum::Router;
}
```

- `handle_request` — Pure state transition. Takes current state + request, returns response + side-effects (events appended to state). No I/O.
- `router` — Returns the Axum router fragment for this twin's domain-specific routes (both native and mimicry). Merged into the full router by `build_twin_router`.

## Session Model

Sessions isolate agent interactions into bounded, replayable units.

1. **Create**: `POST /control/sessions` — optionally provide seed data
2. **Interact**: All subsequent requests are tagged with the session ID
3. **End**: `POST /control/sessions/{id}/end` — freezes the session
4. **Inspect**: Retrieve the session's event stream (`/events`) or final state (`/snapshot`)

Sessions enable A/B testing of agent behavior: run the same scenario in two sessions, compare the event streams.

## Scenario System

Scenarios are YAML documents that script a full test lifecycle:

```yaml
name: "reorg-basic"
seed:
  time: "2025-01-01T00:00:00Z"
  actors: [{ id: "alice", role: "owner" }]
  items:
    - { name: "invoice.pdf", parent: "root", owner: "alice" }
timeline:
  - { at: "T+1s", actor: "alice", action: "create_folder", args: { name: "Invoices" } }
  - { at: "T+2s", actor: "alice", action: "move_item", args: { item: "invoice.pdf", to: "Invoices" } }
faults:
  - { match: { action: "move_item" }, effect: { type: "latency", ms: 500 }, probability: 0.3 }
assertions:
  - { type: "item_exists", path: "/Invoices/invoice.pdf" }
```

**Execution**: Reset from seed → execute timeline in order → evaluate fault rules during each action → run assertions → produce run report with pass/fail, event log, snapshot hash.

**Replay**: Re-execute a run from its persisted bundle and verify the resulting snapshot hash matches the original. This is the determinism gate in CI.

**Runs**: Every scenario execution produces a run artifact persisted to `TWIN_RUNS_DIR`. Runs can be listed, retrieved, bundled for download, replayed, and diffed.

## Auth Model

Authentication maps external tokens to internal actor identities, enabling multi-tenant testing.

**`actors.json`** maps tokens to actor IDs:
```json
{ "tok_alice": "alice", "tok_ci_bot": "ci-bot" }
```

**Resolution chain**: `X-Twin-Actor-Id` header → Bearer token lookup → deterministic hash for unknown tokens → `"default"` fallback. The hash fallback means any token works — unknown ones just get a stable, unique actor ID derived from the token value.

## Extension Points

### Adding a new twin

1. `cargo run --bin twin-cli -- new <name>` — scaffolds crate + server + Dockerfile
2. Define your domain model (items, permissions, whatever your SaaS has)
3. Implement `TwinService` — map requests to state transitions and events
4. Add routes in the `router()` method — native API + optional mimicry routes
5. `cargo run --bin twin-<name>-server` — the control surface, auth, sessions, scenario engine are all inherited from `twin-server-core`

### Adding scenario capabilities

New twins automatically support the scenario engine. Define scenario YAML with seed data and timeline actions that correspond to your `TwinService::Request` variants.

## Docker Packaging

Each twin server gets a multi-stage Dockerfile (`docker/Dockerfile.<name>`):

- **Build stage**: `rust:1.86-bookworm` — compiles the workspace, copies out the binary
- **Runtime stage**: `debian:bookworm-slim` — minimal image with just the binary

`docker-compose.yml` defines services with volume mounts for `runs/` and `scenarios/`, health check on `/health`, and port mapping.

## CI

GitHub Actions (`.github/workflows/ci.yml`):
1. `cargo test --workspace` — all unit and integration tests
2. Scenario validation — ensures all YAML scenarios parse correctly
3. Scenario apply-from-file — runs scenarios end-to-end
4. Replay verification — re-executes persisted runs and asserts determinism (`ok: true`)
