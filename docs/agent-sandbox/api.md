# AgentSandbox API

## CLI

- Primary command: `agent-sandbox`

### Core commands

- `agent-sandbox env up|down`
- `agent-sandbox doctor [--json] [--check-runs]`
- `agent-sandbox capabilities [--json]`
- `agent-sandbox init scenario <name> [--force]`
- `agent-sandbox init run <name> --scenario <id> [--environment <id>] [--target <id>] [--force]`
- `agent-sandbox run list [--tier p0-smoke|p1-deep]`
- `agent-sandbox run targets [--kind workflow|agent]`
- `agent-sandbox run validate <run-id> [--unsafe-plugins]`
- `agent-sandbox run execute <run-id> [--reset] [--assert-after] [--unsafe-plugins]`
- `agent-sandbox run execute-tier <tier> [--unsafe-plugins]`
- `agent-sandbox snapshot [--out <path>]`
- `agent-sandbox observ status`

## DSL v3

### Environment

- Required: `version`, `kind=environment`, `meta`
- Optional:
  - `twins.gmail_base_url`, `twins.drive_base_url`
  - `runtime.env`
  - `plugins` (module list)
  - `plugins_policy`:
    - `allowlist: string[]`
    - `unsafe: bool`

### Scenario

- Required: `version`, `kind=scenario`, `meta`, `seed`, `expect`
- `seed.gmail` and `seed.drive` provide twin seed state
- `expect.assertions[]` + `expect.mode` drive checks

### Run

- Required: `version`, `kind=run`, `meta`, `environment_ref`, `scenario_ref`, `execution`
- `execution.mode`: `workflow|agent`
- `execution.isolation`: `sandbox_only|allow_live` (default runtime behavior: `sandbox_only`)
- Optional adapter mode:
  - `execution.adapter.type`: `command|http`
  - `execution.adapter.config.protocol`: `generic-json|agno-agentos-workflow`
  - `execution.adapter.strict_contract`: `bool` (require adapter HTTP responses to already be `lab.adapter.v1`)

## Environment Variables

- Core paths:
  - `AGENT_SANDBOX_V3_DIR`
  - `AGENT_SANDBOX_SCHEMA_DIR`
- Plugin system:
  - `AGENT_SANDBOX_PLUGIN_MODULES`
  - `AGENT_SANDBOX_PLUGIN_ALLOWLIST`
  - `AGENT_SANDBOX_UNSAFE_PLUGINS`
- Adapter contract:
  - `AGENT_SANDBOX_ADAPTER_STRICT_CONTRACT`
  - `AGENT_SANDBOX_SANDBOX_HTTP_HOSTS`
- Twin endpoints:
  - `AGENT_SANDBOX_TWIN_GMAIL_BASE_URL`
  - `AGENT_SANDBOX_TWIN_DRIVE_BASE_URL`
- Runtime mode:
  - `AGENT_SANDBOX_RUNTIME_MODE`
- Determinism/fault controls:
  - `AGENT_SANDBOX_CLOCK_FIXED_NOW`
  - `AGENT_SANDBOX_RANDOM_SEED`
  - `AGENT_SANDBOX_FAULT_PRESET`
- Observability:
  - `AGENT_SANDBOX_OBSERVABILITY_BASE_URL`

## Policy Precedence

1. CLI explicit flags (for example `--unsafe-plugins`)
2. Environment variables
3. `environment.plugins_policy`

## Compatibility Notes

- Published adapter HTTP contract:
  - `src/agent_sandbox/resources/v3/openapi/adapter-runner.openapi.yaml`
  - Versioning policy: `docs/agent-sandbox/contract-versioning.md`

## Target SDK

- Python helper module: `agent_sandbox.target_sdk`
- Use `load_target_runtime_config()` in target workflows/agents to resolve:
  - mode (`live|twin`)
  - twin Gmail/Drive base URLs
- Optional runtime behavior events:
  - `emit_event(kind, **attrs)` from target runtime code
  - Events are attached to run `session_state.agent_sandbox_events`
  - Assert with `workflow.event_sequence`

