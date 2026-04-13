# agent_sanbox

Reusable workflow and agent sandbox engine for executing workflows and agents against Gmail and Drive digital twins.

## What Lives Here

- `src/agent_sandbox/`: CLI, DSL loader, runtime, assertions, adapters, telemetry, packaged resources
- `docs/agent-sandbox/`: sandbox architecture, contracts, policy, and operational docs
- `tests/`: sandbox-focused unit and contract tests

## Quick Start

```bash
uv sync --all-extras
uv run agent-sandbox doctor --json
```

## Spec Content

Runnable environments, scenarios, and runs live outside this repo. Point the CLI at a spec corpus with `AGENT_SANDBOX_V3_DIR`.

For local development in the current workspace layout:

```bash
$env:AGENT_SANDBOX_V3_DIR='D:\projects\personal\digital-twins-labs\v3'
uv run agent-sandbox run validate first_run_workflow
```
