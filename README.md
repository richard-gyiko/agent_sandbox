# agent_sandbox

A test harness for LLM-powered agents that interact with external services via digital twins. Instead of letting your agent call real APIs during development and testing, it runs against **digital twins** — local mock services that implement the same HTTP contract but keep everything isolated and reproducible.

You write declarative YAML specs that define initial state, run your agent, then assert on what it did.

## Repository Layout

This is a monorepo containing the full stack:

```
src/agent_sandbox/     Python engine — CLI, DSL loader, runtime, adapters, plugin system
sdk/agent_sandbox_twins/  Gmail/Drive twin providers and assertions (Python plugin)
twins/                 Rust twin servers — Gmail and Drive API implementations
  crates/              twin-kernel, twin-service, twin-drive, twin-gmail, etc.
  apps/                twin-drive-server, twin-gmail-server, twin-cli
  docker/              Dockerfiles for twin servers
```

## Why

Testing an AI agent that reads emails, creates files, or manages labels is hard:

- Real API calls are slow, rate-limited, and non-deterministic
- You can't easily control initial state or replay scenarios
- A bug in your agent might delete real emails or share files it shouldn't

Agent Sandbox solves this by giving your agent fake but realistic services to work against, with full control over initial state and assertions to verify behavior.

## How It Works

1. **Define a scenario** (YAML) — seed twins with initial state, declare expected outcomes
2. **Define a run** — link a scenario to your agent/workflow, choose execution mode and isolation level
3. **Execute** — the engine resets twins, seeds them, runs your agent, snapshots final state, and checks assertions

```bash
agent-sandbox run execute my_agent_test --assert-after
```

Your agent connects to the twins via standard API-compatible HTTP endpoints. It doesn't need to know it's in a sandbox.

## Quick Start

```bash
# Install engine + plugin
uv sync --all-extras
pip install -e sdk/agent_sandbox_twins

# Start twin servers
cd twins && docker compose up -d && cd ..

# Check that everything is wired up
export AGENT_SANDBOX_PLUGIN_MODULES=agent_sandbox_twins
uv run agent-sandbox doctor --json
```

## Architecture

The engine is **service-agnostic**. Twin backends are pluggable through the `TwinProvider` protocol — the engine handles orchestration (reset, seed, execute, snapshot, assert) while plugins provide the service-specific logic.

**Gmail and Drive** support is provided by the `agent-sandbox-twins` plugin in `sdk/agent_sandbox_twins/`.

### Engine (Python)

- `TwinProvider` protocol and registry
- DSL v3 spec loading and validation
- Adapter protocol (`lab.adapter.v1`) for HTTP and command execution
- Plugin system for registering providers, assertions, and runners
- Engine-level assertions (workflow state, trace spans)
- `sandbox_only` isolation by default

### Twin Servers (Rust)

- Stateful in-memory replicas of Gmail (v1 API) and Drive (v3 API)
- Deterministic replay, fault injection, session isolation
- Control surface: `/control/reset`, `/control/seed`, `/control/snapshot`, `/control/events`
- Docker images published to GHCR

See `twins/README.md` and `twins/ARCHITECTURE.md` for details.

## CLI Commands

```bash
agent-sandbox env up|down              # Start/stop digital twins (Docker Compose)
agent-sandbox doctor                   # Health check (twins, observability, plugins)
agent-sandbox capabilities             # List registered targets, assertions, actions
agent-sandbox init scenario <name>     # Scaffold a new scenario
agent-sandbox init run <name>          # Scaffold a new run
agent-sandbox run list                 # List available runs
agent-sandbox run validate <run-id>    # Validate spec without executing
agent-sandbox run execute <run-id>     # Execute a run
agent-sandbox run execute-tier p0-smoke # Execute all runs in a tier
agent-sandbox snapshot                 # Snapshot current twin state
```

## License

[MIT](LICENSE)
