# AGENTS.md

This file gives coding agents a fast orientation to the repo.

## What This Project Is

`agent_sanbox` is the reusable sandbox engine for running workflows and agents against Gmail and Drive digital twins using declarative specs.

It owns:
- the `agent-sandbox` CLI
- DSL/spec loading and validation
- execution runtime and target dispatch
- adapter and assertion contracts
- packaged schema/OpenAPI resources
- generic sandbox documentation

Runnable spec content does not live here. That belongs in the separate `digital-twins-labs` repo and is discovered through `AGENT_SANDBOX_V3_DIR`.

## Main Code Areas

- `src/agent_sandbox/cli.py`
- `src/agent_sandbox/dsl.py`
- `src/agent_sandbox/runner.py`
- `src/agent_sandbox/schema.py`
- `src/agent_sandbox/adapters.py`
- `src/agent_sandbox/assertion_handlers.py`
- `src/agent_sandbox/resources/v3/`
- `docs/agent-sandbox/`
- `tests/`

## Source Of Truth Rules

- Schemas and OpenAPI under `src/agent_sandbox/resources/v3/` are canonical.
- Generic sandbox docs belong in `docs/agent-sandbox/`.
- Runnable environments, scenarios, and runs belong in the `digital-twins-labs` repo, not here.
- Product-specific target registration belongs in consuming repos such as `whizy`, not here.

## Common Commands

```bash
uv sync --all-extras
uv run pytest
uv run ruff check .
uv run agent-sandbox doctor --json
```

## Runtime Notes

- The CLI discovers specs from `AGENT_SANDBOX_V3_DIR`.
- In the current local workspace layout that should point at `D:\projects\personal\digital-twins-labs\v3`.
- Keep the package import name `agent_sandbox` stable even though the repo is named `agent_sanbox`.

## Expected Quality Bar

- Keep the engine independent from product repos.
- Do not reintroduce direct imports from `whizy` or other consumers.
- Prefer updating `README.md` and this file when the repo structure or workflow changes materially.
