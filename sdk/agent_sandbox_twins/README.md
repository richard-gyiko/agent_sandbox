# agent-sandbox-twins

Gmail and Drive twin providers for [agent-sandbox](https://github.com/richard-gyiko/agent_sanbox).

This package registers Gmail and Drive as twin backends, providing:

- Twin lifecycle operations (health check, reset, seed, snapshot)
- Gmail/Drive-specific assertion handlers (file exists, label applied, op count, etc.)
- Snapshot reshaping from raw twin state to the format assertions expect

## Installation

```bash
pip install agent-sandbox-twins
```

## Usage

Add to your environment spec:

```yaml
plugins:
  - agent_sandbox_twins
```

Or set the environment variable:

```bash
export AGENT_SANDBOX_PLUGIN_MODULES=agent_sandbox_twins
```

The plugin auto-registers Gmail and Drive providers on import.
