"""Built-in action handlers for AgentSandbox scenarios."""

from __future__ import annotations

from typing import Any


def noop(_action: dict[str, Any], _context: Any) -> None:
    return


def register_default_actions(*, register_action, register_action_param_schema) -> None:
    register_action("noop", noop)
    register_action_param_schema("noop", "actions/noop.params.schema.json")
