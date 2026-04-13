"""Validation API surface and policy helpers for AgentSandbox."""

from __future__ import annotations

from typing import Any


def plugin_policy_env(env_spec: dict[str, Any]) -> dict[str, str]:
    """Resolve plugin policy environment overrides from environment spec."""
    overrides: dict[str, str] = {}
    policy = env_spec.get("plugins_policy", {})
    if not isinstance(policy, dict):
        return overrides

    allowlist = policy.get("allowlist", [])
    if isinstance(allowlist, list):
        items = [str(item).strip() for item in allowlist if str(item).strip()]
        if items:
            overrides["AGENT_SANDBOX_PLUGIN_ALLOWLIST"] = ",".join(items)

    if "unsafe" in policy:
        overrides["AGENT_SANDBOX_UNSAFE_PLUGINS"] = "true" if bool(policy["unsafe"]) else "false"
    return overrides


def validate_run_spec(run_spec: dict[str, Any]) -> dict[str, Any]:
    # Local import avoids module cycle while runner still hosts validation engine internals.
    from agent_sandbox import runner

    return runner.validate_run_spec(run_spec)
