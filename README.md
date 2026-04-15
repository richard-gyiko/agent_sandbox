# agent_sandbox

A test harness for LLM-powered agents that interact with external services via digital twins. Instead of letting your agent call real APIs during development and testing, it runs against **digital twins** — local mock services that implement the same HTTP contract but keep everything isolated and reproducible.

You write declarative YAML specs that define initial state, run your agent, then assert on what it did.

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

## Architecture

The engine is **service-agnostic**. Twin backends are pluggable through the `TwinProvider` protocol — the engine handles orchestration (reset, seed, execute, snapshot, assert) while plugins provide the service-specific logic.

**Gmail and Drive** support is provided by the [`agent-sandbox-twins`](https://github.com/richard-gyiko/digital-twins/tree/main/sdk/python) plugin, which lives in the [digital-twins](https://github.com/richard-gyiko/digital-twins) repo alongside the twin servers themselves.

## Quick Start

```bash
# Install engine + Gmail/Drive plugin
uv sync --all-extras
pip install agent-sandbox-twins

# Activate the plugin
export AGENT_SANDBOX_PLUGIN_MODULES=agent_sandbox_twins

# Point at your spec corpus (scenarios, environments, runs)
export AGENT_SANDBOX_V3_DIR='/path/to/digital-twins/v3'

# Check that everything is wired up
uv run agent-sandbox doctor --json

# Validate and run
uv run agent-sandbox run validate my_first_run
uv run agent-sandbox run execute my_first_run --assert-after
```

Spec content (environments, scenarios, runs) lives in the [digital-twins](https://github.com/richard-gyiko/digital-twins) repo. This repo is the engine only.

## What's in the Engine

**Orchestration:** generic twin lifecycle via `TwinProvider` plugins — health check, reset, seed, snapshot, event collection.

**Engine-level assertions:**
- **Workflow** — session state checks, metric equals, event sequence
- **Trace** — span exists, span attribute equals (requires DuckLens)

**Execution modes:**
- **Local** — run registered workflow/agent handlers directly in-process
- **HTTP adapter** — POST a `lab.adapter.v1` payload to a remote service
- **Command adapter** — spawn a subprocess with the payload on stdin

**Safety:** `sandbox_only` isolation by default (blocks real API access). Explicit opt-in required for `allow_live` mode. Plugin loading governed by allowlist policy.

## What's in the Plugin

The [`agent-sandbox-twins`](https://github.com/richard-gyiko/digital-twins/tree/main/sdk/python) plugin provides:

- Gmail and Drive `TwinProvider` implementations
- Gmail/Drive assertions (file exists, label applied, op count, filename sequence, etc.)
- Snapshot reshaping from raw twin state

## Key Concepts

| Concept | What it is |
|---------|-----------|
| **Digital twin** | Local mock HTTP server implementing a real service's API |
| **TwinProvider** | Plugin that teaches the engine how to manage a specific twin type |
| **Environment** | Configuration for twin endpoints, plugins, and runtime settings |
| **Scenario** | Test case: seed state + expected assertions |
| **Run** | Executable spec linking an environment + scenario + target agent |
| **Adapter** | Bridge to invoke external agents via HTTP or shell command |

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

## Project Structure

```
src/agent_sandbox/     CLI, DSL loader, runtime, adapters, assertions, telemetry
  twin_provider.py     TwinProvider protocol and registry
  resources/v3/        Packaged JSON schemas and OpenAPI specs
docs/agent-sandbox/    Architecture, contracts, and safety model
tests/                 Unit and contract tests
```

## License

[MIT](LICENSE)
