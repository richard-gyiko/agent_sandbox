# Plugin Interface v1

Agent Sandbox plugins are Python modules loaded at run validation/execution time.
They self-register capabilities through the public `agent_sandbox.runner` registration APIs.

## Registration APIs

- Workflow execution targets:
  - `register_workflow_runner(target: str, handler)`
- Agent execution targets:
  - `register_agent_runner(agent_id: str, handler, aliases=())`
- Assertions:
  - `register_assertion(kind: str, handler)`
  - `register_assertion_param_schema(kind: str, schema_relpath: str)`
- Actions:
  - `register_action(kind: str, handler)`
  - `register_action_param_schema(kind: str, schema_relpath: str)`
- HTTP adapter protocols:
  - `register_http_protocol(name: str, handler)`
  - `register_http_protocol_config_schema(protocol: str, schema_relpath: str)`

## Loading Model

- Plugins are loaded from:
  - `environment.plugins`
  - `run.plugins`
  - optional `AGENT_SANDBOX_PLUGIN_MODULES`
- Safety policy:
  - allowlisted modules via `AGENT_SANDBOX_PLUGIN_ALLOWLIST`
  - or explicit unsafe mode `AGENT_SANDBOX_UNSAFE_PLUGINS=true`

## Validation Expectations

1. If run execution points to a plugin-provided target, plugin import must register it.
2. Missing registration fails preflight (`run validate`) with unknown target/agent.
3. Assertion/action kinds referenced by scenarios must be registered before validation.
4. Plugin schema paths must point to valid bundled schemas.

## Minimal Workflow Plugin Example

```python
from agent_sandbox.runner import register_workflow_runner


def _handler(scenario, endpoints, session_id):
    return {"status": "COMPLETED", "content": "ok", "session_state": {}}


register_workflow_runner("workflow.external.example", _handler)
```
