# Digital Twins

Test AI agents against SaaS service replicas without hitting production.

## What Is This?

Digital Twins is a Rust platform that hosts stateful, in-memory replicas of SaaS services. Agents interact with a twin exactly as they would with the real service — same REST endpoints, same JSON payloads — but every state change is captured as a deterministic event stream. You can snapshot, replay, inject faults, and run scripted scenarios, all without credentials or rate limits.

**Available twins:**

- **Google Drive** — Drive v3-compatible surface: create files, list folders, manage permissions, upload content. Point your SDK at `http://localhost:8080/drive/v3/` instead of `https://www.googleapis.com`.
- **Gmail** — Gmail v1-compatible surface: messages, threads, labels, attachments. Point your SDK at `http://localhost:8081/gmail/v1/` instead of `https://gmail.googleapis.com`.

## Quick Start

### Cargo

```sh
cargo run --bin twin-drive-server
# Drive twin listening on http://localhost:8080

cargo run --bin twin-gmail-server
# Gmail twin listening on http://localhost:8080
```

### Docker

```sh
docker compose up              # both twins
docker compose up twin-drive   # just Drive (port 8080)
docker compose up twin-gmail   # just Gmail (port 8081)
```

### Docker Images From GHCR

Published images are available from GitHub Container Registry:

```sh
docker pull ghcr.io/richard-gyiko/twin-drive-server:main
docker run --rm -p 8080:8080 ghcr.io/richard-gyiko/twin-drive-server:main
```

```sh
docker pull ghcr.io/richard-gyiko/twin-gmail-server:main
docker run --rm -p 8081:8080 ghcr.io/richard-gyiko/twin-gmail-server:main
```

Version tags such as `1.2.3` and `latest` are published when a Git tag like `v1.2.3` is pushed.

### Run the example agent

```sh
# In a second terminal
uv run examples/agno_drive_reorganizer.py
```

The example resets the twin, seeds files, and runs a decision loop that organizes them into folders.

## Architecture

```
twin-kernel          Pure state machine (events, snapshots, replay)
     |
twin-service         TwinService trait + runtime wrapper
     |
twin-scenario        Scenario DSL schema (seed, timeline, faults, assertions)
     |
twin-server-core     Generic HTTP host: control routes, auth, scenario engine
     |                    \
twin-drive             twin-<future>       Twin implementations (domain model + routes)
     |                    \
twin-drive-server      twin-<future>-server   Thin binaries
```

Each layer depends only on the layers above it. `twin-kernel` has no external dependencies beyond `serde`. See [ARCHITECTURE.md](ARCHITECTURE.md) for the full breakdown.

## How Agents Connect

**SDK-based agents** (e.g. Google's Python client): change the base URL. The twins implement the same REST contracts as the real services.

- Drive: `http://localhost:8080/drive/v3/` — `GET /drive/v3/files`, `POST /drive/v3/files`, `PATCH /drive/v3/files/{id}`, etc.
- Gmail: `http://localhost:8081/gmail/v1/` — `GET /gmail/v1/users/me/messages`, `POST /gmail/v1/users/me/messages/send`, etc.

**Custom clients**: use the twin-native routes (Drive: `/drive/folders`, `/drive/files`, `/drive/items/*`; Gmail: `/gmail/messages/*`, `/gmail/labels`) which offer a simpler, more explicit API without Google API conventions.

**State inspection**: `GET /state/items` and `GET /state/tree` let tests examine internal state directly.

## Authentication

The twin maps Bearer tokens to actor IDs via an `actors.json` file:

```json
{
  "tok_alice": "alice",
  "tok_bob": "bob"
}
```

Resolution priority:
1. `X-Twin-Actor-Id` header (explicit override)
2. `Authorization: Bearer <token>` mapped through `actors.json`
3. Unknown tokens get a deterministic hash-based ID: `actor_<16 hex chars>`
4. No credentials at all → actor `"default"`

Set `TWIN_AUTH_FILE` to point to your mapping file (default: `./actors.json`).

## Creating a New Twin

```sh
cargo run --bin twin-cli -- new calendar
```

This scaffolds:
- `crates/twin-calendar/` — domain model crate with a `TwinService` implementation stub
- `apps/twin-calendar-server/` — thin binary that wires the service into the generic HTTP host
- `docker/Dockerfile.calendar` — multi-stage build
- `scenarios/calendar/` — directory for scenario YAML files
- Updates the workspace `Cargo.toml`

Implement `TwinService` for your domain and add routes — the control surface, auth, sessions, and scenario engine come for free from `twin-server-core`.

## Session Workflow

Sessions isolate agent interactions into replayable units:

```
POST   /control/sessions              → create session (optional seed data)
       ... interact via Drive / v3 routes ...
POST   /control/sessions/{id}/end     → freeze the session
GET    /control/sessions/{id}/events  → retrieve event stream
GET    /control/sessions/{id}/snapshot → get final state snapshot
```

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `TWIN_PORT` | `8080` | HTTP listen port |
| `TWIN_SCENARIOS_DIR` | `./scenarios` | Scenario YAML file directory |
| `TWIN_RUNS_DIR` | `./runs` | Run artifact persistence directory |
| `TWIN_LOG_LEVEL` | `info` | Tracing log level (overrides `RUST_LOG`) |
| `TWIN_AUTH_FILE` | `./actors.json` | Token-to-actor mapping file |

## API Overview

### Control Surface

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Health check |
| POST | `/control/reset` | Reset all state |
| GET | `/control/snapshot` | Export current state |
| POST | `/control/restore` | Restore from snapshot |
| GET | `/control/events` | Full event log |
| POST | `/control/scenario/validate` | Validate scenario YAML |
| POST | `/control/scenario/apply` | Run scenario (inline) |
| POST | `/control/scenario/apply-file` | Run scenario (from file) |
| POST | `/control/scenario/replay` | Deterministic replay |
| GET | `/control/scenario/runs` | List past runs |
| GET | `/control/scenario/runs/{id}` | Get run report |
| GET | `/control/scenario/runs/{id}/bundle` | Download run bundle |
| POST | `/control/scenario/runs/{id}/verify-replay` | Verify replay determinism |
| POST | `/control/scenario/runs/diff` | Diff two runs |
| GET/POST | `/control/sessions` | List / create sessions |
| GET | `/control/sessions/{id}` | Get session details |
| POST | `/control/sessions/{id}/end` | End session |
| GET | `/control/sessions/{id}/events` | Session events |
| GET | `/control/sessions/{id}/snapshot` | Session snapshot |

### Drive Twin — Native Routes

| Method | Path | Purpose |
|---|---|---|
| POST | `/drive/folders` | Create folder |
| POST | `/drive/files` | Create file |
| GET | `/drive/items/{parent_id}/children` | List children |
| POST | `/drive/items/{item_id}/permissions` | Add permission |
| POST | `/drive/items/{item_id}/move` | Move item |
| GET | `/drive/items/{item_id}` | Get item |
| DELETE | `/drive/items/{item_id}` | Delete item |

### Drive Twin — Google v3 Mimicry

| Method | Path | Purpose |
|---|---|---|
| GET | `/drive/v3/files` | List files (supports `q` filter) |
| POST | `/drive/v3/files` | Create file/folder |
| GET | `/drive/v3/files/{id}` | Get file metadata (`alt=media` for content) |
| PATCH | `/drive/v3/files/{id}` | Update file metadata |
| DELETE | `/drive/v3/files/{id}` | Delete file |
| POST | `/drive/v3/files/{id}/permissions` | Add permission |
| POST | `/upload/drive/v3/files` | Upload file (media/multipart/resumable) |
| PUT | `/upload/drive/v3/files` | Resumable upload chunk |

### Gmail Twin — Native Routes

| Method | Path | Purpose |
|---|---|---|
| POST | `/gmail/messages/send` | Send message |
| GET | `/gmail/messages/{id}` | Get message |
| DELETE | `/gmail/messages/{id}` | Delete message |
| POST | `/gmail/messages/{id}/labels` | Modify labels |
| GET | `/gmail/labels` | List labels |
| POST | `/gmail/labels` | Create label |
| DELETE | `/gmail/labels/{id}` | Delete label |
| GET | `/gmail/threads/{id}` | Get thread |

### Gmail Twin — Google v1 Mimicry

| Method | Path | Purpose |
|---|---|---|
| GET | `/gmail/v1/users/me/messages` | List messages (supports `q` filter) |
| POST | `/gmail/v1/users/me/messages` | Insert message |
| GET | `/gmail/v1/users/me/messages/{id}` | Get message |
| DELETE | `/gmail/v1/users/me/messages/{id}` | Delete message |
| POST | `/gmail/v1/users/me/messages/send` | Send message |
| POST | `/gmail/v1/users/me/messages/{id}/modify` | Modify labels |
| POST | `/gmail/v1/users/me/messages/{id}/trash` | Trash message |
| POST | `/gmail/v1/users/me/messages/{id}/untrash` | Untrash message |
| GET | `/gmail/v1/users/me/threads` | List threads |
| GET | `/gmail/v1/users/me/threads/{id}` | Get thread |
| POST | `/gmail/v1/users/me/threads/{id}/modify` | Modify thread labels |
| POST | `/gmail/v1/users/me/threads/{id}/trash` | Trash thread |
| POST | `/gmail/v1/users/me/threads/{id}/untrash` | Untrash thread |
| DELETE | `/gmail/v1/users/me/threads/{id}` | Delete thread |
| GET | `/gmail/v1/users/me/labels` | List labels |
| POST | `/gmail/v1/users/me/labels` | Create label |
| GET | `/gmail/v1/users/me/labels/{id}` | Get label |
| PUT | `/gmail/v1/users/me/labels/{id}` | Update label |
| PATCH | `/gmail/v1/users/me/labels/{id}` | Patch label |
| DELETE | `/gmail/v1/users/me/labels/{id}` | Delete label |
| GET | `/gmail/v1/users/me/messages/{id}/attachments/{att_id}` | Get attachment |
| GET | `/gmail/v1/users/me/profile` | Get profile |

### State Inspection

| Method | Path | Purpose |
|---|---|---|
| GET | `/state/items` | All items (flat) |
| GET | `/state/items/{id}` | Single item |
| GET | `/state/tree` | Full tree view |

## Examples

Two Python examples live in `examples/`:

- **`agno_drive_reorganizer.py`** — Uses twin-native APIs to organize files (no LLM needed)
- **`agno_twin_drive_agent.py`** — Real Agno agent with OpenAI that calls the twin

```sh
# Twin-native example (no API key needed)
uv run examples/agno_drive_reorganizer.py

# Agno + OpenAI against the twin
OPENAI_API_KEY=your_key uv run --with agno --with openai examples/agno_twin_drive_agent.py
```

## Running Tests

```sh
cargo test --workspace
```

## CI

GitHub Actions (`.github/workflows/ci.yml`) runs workspace tests, scenario validation, scenario apply-from-file, and replay verification.

## License

MIT
