# agent_sandbox

A test harness for LLM-powered agents. Instead of letting your agent call real APIs during development and testing, it runs against **digital twins** — local mock services that behave like the real thing but keep everything isolated and reproducible.

You write declarative specs that define initial state, run your agent, then assert on what it did.

The engine is service-agnostic — any API can be twin'd. Gmail and Drive twins are the current implementations available in the [digital-twins](https://github.com/richard-gyiko/digital-twins) repo.

## Why

Testing an AI agent that interacts with external services is hard:

- Real API calls are slow, rate-limited, and non-deterministic
- You can't easily control initial state or replay scenarios
- A bug in your agent might cause real side effects (deleted data, sent emails, shared files)

Agent Sandbox solves this by giving your agent a fake but realistic service to work against, with full control over initial state and built-in assertions to verify behavior.

## How It Works

1. **Define a scenario** (YAML) — seed twins with initial state and declare expected outcomes
2. **Define a run** — link a scenario to your agent/workflow, choose execution mode and isolation level
3. **Execute** — the engine resets twins, seeds them, runs your agent, snapshots final state, and checks assertions

```bash
agent-sandbox run execute my_agent_test --assert-after
```

Your agent connects to the twins via standard API-compatible HTTP endpoints. It doesn't need to know it's in a sandbox.

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

## Key Concepts

| Concept | What it is |
|---------|-----------|
| **Digital twin** | Local mock HTTP service that implements a real API's contract |
| **Environment** | Configuration for twin endpoints, plugins, and runtime settings |
| **Scenario** | Test case: seed state + expected assertions |
| **Run** | Executable spec linking an environment + scenario + target agent |
| **Adapter** | Bridge to invoke external agents via HTTP or shell command |
| **Assertion** | Check on final twin state (e.g., file exists, label applied, op count) |

## CLI Commands

```bash
agent-sandbox env up|down              # Start/stop digital twins
agent-sandbox doctor                   # Health check
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
