# AGENTS.md

This file gives coding agents a fast orientation to the repo.

## What This Project Is

`agent_sandbox` is a generic sandbox engine for running and testing LLM-powered workflows and agents against digital twins using declarative YAML specs.

The engine is **service-agnostic** — it orchestrates twin lifecycle (reset, seed, execute, snapshot, assert) through a `TwinProvider` plugin interface. Gmail and Drive support is provided by the separate [`agent-sandbox-twins`](https://github.com/richard-gyiko/digital-twins/tree/main/sdk/python) plugin package in the digital-twins repo.

It owns:
- the `agent-sandbox` CLI
- the `TwinProvider` protocol and registry (`twin_provider.py`)
- DSL v3 spec loading and validation
- execution runtime and target dispatch
- adapter protocol (`lab.adapter.v1`)
- plugin system for registering providers, assertions, and runners
- engine-level assertions (workflow, trace) and actions
- packaged schema/OpenAPI resources
- generic sandbox documentation

It does **not** own:
- Gmail/Drive twin providers, assertions, or schemas (those live in `agent-sandbox-twins`)
- Twin server implementations (Rust, in `digital-twins`)
- Runnable spec content (environments, scenarios, runs — in `digital-twins`)

## Main Code Areas

- `src/agent_sandbox/twin_provider.py` — TwinProvider protocol and registry
- `src/agent_sandbox/runner.py` — orchestration, twin lifecycle, execution dispatch
- `src/agent_sandbox/cli.py` — CLI commands
- `src/agent_sandbox/adapters.py` — adapter protocol (HTTP, command)
- `src/agent_sandbox/assertion_handlers.py` — engine-level assertions (workflow.*, trace.*)
- `src/agent_sandbox/execution_registry.py` — global handler registries
- `src/agent_sandbox/plugins.py` — plugin discovery and loading
- `src/agent_sandbox/target_sdk.py` — runtime config and event hooks for target code
- `src/agent_sandbox/schema.py` — JSON schema loading and validation
- `src/agent_sandbox/resources/v3/` — packaged schemas and OpenAPI specs
- `docs/agent-sandbox/` — architecture, contracts, safety model
- `tests/` — unit and contract tests

## Source Of Truth Rules

- Schemas and OpenAPI under `src/agent_sandbox/resources/v3/` are canonical for engine-level specs.
- Gmail/Drive assertion schemas live in the `agent-sandbox-twins` package, not here.
- Generic sandbox docs belong in `docs/agent-sandbox/`.
- Runnable environments, scenarios, and runs belong in the [`digital-twins`](https://github.com/richard-gyiko/digital-twins) repo, not here.
- Product-specific target registration belongs in consuming repos, not here.

## Common Commands

```bash
uv sync --all-extras
uv run pytest
uv run ruff check .
uv run agent-sandbox doctor --json
```

## Runtime Notes

- The CLI discovers specs from `AGENT_SANDBOX_V3_DIR`.
- In a typical local workspace layout that should point at the `v3` directory of the [`digital-twins`](https://github.com/richard-gyiko/digital-twins) repo.
- Twin providers must be registered before execution. Install `agent-sandbox-twins` or set `AGENT_SANDBOX_PLUGIN_MODULES=agent_sandbox_twins`.
- Keep the package import name `agent_sandbox` stable.

## Expected Quality Bar

- Keep the engine independent from specific twin implementations.
- Do not add Gmail/Drive-specific code — that belongs in the twin plugin.
- Do not introduce direct imports from consuming repos.
- Prefer updating `README.md` and this file when the repo structure or workflow changes materially.
