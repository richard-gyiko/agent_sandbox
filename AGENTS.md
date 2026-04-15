# AGENTS.md

This file gives coding agents a fast orientation to the repo.

## What This Project Is

A monorepo containing a test harness for LLM-powered agents, with digital twin servers for Gmail and Drive.

## Repo Structure

- **Engine** (`src/agent_sandbox/`) — Python. Generic sandbox orchestration via `TwinProvider` plugins.
- **Twin plugin** (`sdk/agent_sandbox_twins/`) — Python. Gmail/Drive providers, assertions, snapshot reshaping.
- **Twin servers** (`twins/`) — Rust. Stateful in-memory Gmail and Drive API replicas.

## Main Code Areas

### Engine (Python)
- `src/agent_sandbox/twin_provider.py` — TwinProvider protocol and registry
- `src/agent_sandbox/runner.py` — orchestration, twin lifecycle, execution dispatch
- `src/agent_sandbox/cli.py` — CLI commands
- `src/agent_sandbox/adapters.py` — adapter protocol (HTTP, command)
- `src/agent_sandbox/assertion_handlers.py` — engine-level assertions (workflow.*, trace.*)
- `src/agent_sandbox/execution_registry.py` — global handler registries
- `src/agent_sandbox/plugins.py` — plugin discovery and loading
- `src/agent_sandbox/schema.py` — JSON schema loading (supports multi-package resolution)
- `src/agent_sandbox/resources/v3/` — packaged schemas and OpenAPI specs
- `docs/agent-sandbox/` — architecture, contracts, safety model
- `tests/` — engine unit and contract tests

### Twin Plugin (Python)
- `sdk/agent_sandbox_twins/src/agent_sandbox_twins/` — providers and assertions
- `sdk/agent_sandbox_twins/src/agent_sandbox_twins/resources/` — assertion param schemas

### Twin Servers (Rust)
- `twins/crates/` — library crates (kernel, service, drive, gmail, etc.)
- `twins/apps/` — server binaries and CLI
- `twins/docker/` — Dockerfiles
- `twins/specs/` — TOML API specs for code generation
- `twins/scenarios/` — test scenarios
- `twins/ARCHITECTURE.md` — layered design docs

## Common Commands

```bash
# Python engine
uv sync --all-extras
uv run pytest
uv run ruff check .

# Twin servers
cd twins && cargo test --workspace
cd twins && docker compose up -d

# Full stack
export AGENT_SANDBOX_PLUGIN_MODULES=agent_sandbox_twins
uv run agent-sandbox doctor --json
```

## Source Of Truth Rules

- Engine schemas under `src/agent_sandbox/resources/v3/` are canonical for engine-level specs.
- Gmail/Drive assertion schemas live in `sdk/agent_sandbox_twins/`, not in the engine.
- Twin server code lives under `twins/`. Rust workspace commands run from `twins/`.
- Do not add Gmail/Drive-specific code to the engine — that belongs in the twin plugin.
- Do not introduce direct imports between the engine and twin server code.

## Expected Quality Bar

- Keep the engine independent from specific twin implementations.
- Keep `twins/` self-contained — Cargo and Docker commands work from within it.
- Prefer updating `README.md` and this file when structure changes materially.
