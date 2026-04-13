"""Registry/capabilities API surface for AgentSandbox."""

from __future__ import annotations

from typing import Any

from agent_sandbox import runner


def register_assertion(kind: str, handler) -> None:
    runner.register_assertion(kind, handler)


def register_action(kind: str, handler) -> None:
    runner.register_action(kind, handler)


def register_workflow_runner(target: str, handler) -> None:
    runner.register_workflow_runner(target, handler)


def register_agent_runner(agent_id: str, handler, aliases: tuple[str, ...] = ()) -> None:
    runner.register_agent_runner(agent_id, handler, aliases=aliases)


def register_http_protocol(name: str, handler) -> None:
    runner.register_http_protocol(name, handler)


def register_assertion_param_schema(kind: str, schema_relpath: str) -> None:
    runner.register_assertion_param_schema(kind, schema_relpath)


def register_action_param_schema(kind: str, schema_relpath: str) -> None:
    runner.register_action_param_schema(kind, schema_relpath)


def register_http_protocol_config_schema(protocol: str, schema_relpath: str) -> None:
    runner.register_http_protocol_config_schema(protocol, schema_relpath)


def load_execution_plugins(module_names: list[str]) -> None:
    runner.load_execution_plugins(module_names)


def list_registered_targets(kind: str | None = None) -> list[str]:
    return runner.list_registered_targets(kind=kind)


def list_capabilities() -> dict[str, Any]:
    return runner.list_capabilities()
