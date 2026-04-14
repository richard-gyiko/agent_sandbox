# agent_sandbox

A test harness for LLM-powered agents that interact with Gmail and Drive. Instead of letting your agent call real Google APIs during development and testing, it runs against **digital twins** — local mock services that implement the same HTTP contract but keep everything isolated and reproducible.

You write declarative YAML specs that define initial state (emails, files), run your agent, then assert on what it did.

## Why

Testing an AI agent that reads emails, creates files, or manages labels is hard:

- Real API calls are slow, rate-limited, and non-deterministic
- You can't easily control initial state or replay scenarios
- A bug in your agent might delete real emails or share files it shouldn't

Agent Sandbox solves this by giving your agent fake but realistic Gmail and Drive services to work against, with full control over initial state and built-in assertions to verify behavior.

## How It Works

1. **Define a scenario** (YAML) — seed Gmail/Drive twins with initial emails and files, declare expected outcomes
2. **Define a run** — link a scenario to your agent/workflow, choose execution mode and isolation level
3. **Execute** — the engine resets twins, seeds them, runs your agent, snapshots final state, and checks assertions

```bash
agent-sandbox run execute my_agent_test --assert-after
```

Your agent connects to the twins via standard Google API-compatible HTTP endpoints. It doesn't need to know it's in a sandbox.

## Quick Start

```bash
# Install
uv sync --all-extras

# Check that everything is wired up
uv run agent-sandbox doctor --json

# Point at your spec corpus (scenarios, environments, runs)
export AGENT_SANDBOX_V3_DIR='/path/to/digital-twins/v3'

# Validate and run
uv run agent-sandbox run validate my_first_run
uv run agent-sandbox run execute my_first_run --assert-after
```

Spec content (environments, scenarios, runs) lives in the [digital-twins](https://github.com/richard-gyiko/digital-twins) repo. This repo is the engine only.

## What's Built In

**Twin support:** Gmail and Drive. The core engine (seeding, snapshotting, reset) currently assumes these two services. The plugin system and adapter contract are designed to be extended to other services, but adding a new twin type would require changes to the runner and snapshot logic, not just a plugin.

**Assertions:**
- **Drive** — file exists, no file under path, file hash, operation count, filename sequence validation
- **Gmail** — message has label, operation count
- **Workflow** — session state checks, metric equals, event sequence
- **Trace** — span exists, span attribute equals (requires DuckLens)

**Execution modes:**
- **Local** — run registered workflow/agent handlers directly in-process
- **HTTP adapter** — POST a `lab.adapter.v1` payload to a remote service
- **Command adapter** — spawn a subprocess with the payload on stdin

**Safety:** `sandbox_only` isolation by default (blocks real API access). Explicit opt-in required for `allow_live` mode. Plugin loading governed by allowlist policy.

## Key Concepts

| Concept | What it is |
|---------|-----------|
| **Digital twin** | Local mock HTTP server implementing Gmail or Drive APIs |
| **Environment** | Configuration for twin endpoints, plugins, and runtime settings |
| **Scenario** | Test case: seed state (emails, files) + expected assertions |
| **Run** | Executable spec linking an environment + scenario + target agent |
| **Adapter** | Bridge to invoke external agents via HTTP or shell command |
| **Plugin** | Python module that registers custom workflow runners, agent runners, or assertions |

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
  resources/v3/        Packaged JSON schemas and OpenAPI specs
docs/agent-sandbox/    Architecture, contracts, and safety model
tests/                 Unit and contract tests
```

## License

[MIT](LICENSE)
