"""Execution target/assertion/action registry state for AgentSandbox."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

_ASSERTIONS: dict[str, Any] = {}
_ACTIONS: dict[str, Any] = {}
_WORKFLOW_RUNNERS: dict[str, Any] = {}
_AGENT_RUNNERS: dict[str, Any] = {}
_AGENT_CANONICAL: dict[str, str] = {}
_ASSERTION_PARAM_SCHEMAS: dict[str, tuple[str, str]] = {}  # kind -> (relpath, package)
_ACTION_PARAM_SCHEMAS: dict[str, tuple[str, str]] = {}  # kind -> (relpath, package)


@dataclass
class ExecutionRegistryState:
    ready: bool = False


_STATE = ExecutionRegistryState()


def register_assertion(kind: str, handler: Any) -> None:
    _ASSERTIONS[kind] = handler


def register_action(kind: str, handler: Any) -> None:
    _ACTIONS[kind] = handler


def register_workflow_runner(target: str, handler: Any) -> None:
    _WORKFLOW_RUNNERS[target] = handler


def register_agent_runner(canonical: str, handler: Any, aliases: tuple[str, ...] = ()) -> None:
    _AGENT_RUNNERS[canonical] = handler
    _AGENT_CANONICAL[canonical] = canonical
    for alias in aliases:
        _AGENT_RUNNERS[alias] = handler
        _AGENT_CANONICAL[alias] = canonical


def register_assertion_param_schema(
    kind: str, schema_relpath: str, *, package: str = "agent_sandbox"
) -> None:
    _ASSERTION_PARAM_SCHEMAS[kind] = (schema_relpath, package)


def register_action_param_schema(
    kind: str, schema_relpath: str, *, package: str = "agent_sandbox"
) -> None:
    _ACTION_PARAM_SCHEMAS[kind] = (schema_relpath, package)


def has_assertion(kind: str) -> bool:
    return kind in _ASSERTIONS


def has_action(kind: str) -> bool:
    return kind in _ACTIONS


def has_workflow_target(target: str) -> bool:
    return target in _WORKFLOW_RUNNERS


def has_agent_runner(agent_id: str) -> bool:
    return agent_id in _AGENT_RUNNERS


def get_assertion(kind: str) -> Any | None:
    return _ASSERTIONS.get(kind)


def get_action(kind: str) -> Any | None:
    return _ACTIONS.get(kind)


def get_workflow_runner(target: str) -> Any | None:
    return _WORKFLOW_RUNNERS.get(target)


def get_agent_runner(agent_id: str) -> Any | None:
    return _AGENT_RUNNERS.get(agent_id)


def get_agent_canonical(agent_id: str) -> str:
    return _AGENT_CANONICAL.get(agent_id, agent_id)


def get_assertion_param_schema(kind: str) -> tuple[str, str]:
    return _ASSERTION_PARAM_SCHEMAS.get(kind, ("", "agent_sandbox"))


def get_action_param_schema(kind: str) -> tuple[str, str]:
    return _ACTION_PARAM_SCHEMAS.get(kind, ("", "agent_sandbox"))


def list_assertion_kinds() -> list[str]:
    return sorted(_ASSERTIONS.keys())


def list_action_kinds() -> list[str]:
    return sorted(_ACTIONS.keys())


def list_workflow_targets() -> list[str]:
    return sorted(_WORKFLOW_RUNNERS.keys())


def list_agent_canonical_ids() -> list[str]:
    return sorted(set(_AGENT_CANONICAL.values()))


def list_assertion_param_schema_kinds() -> list[str]:
    return sorted(_ASSERTION_PARAM_SCHEMAS.keys())


def list_action_param_schema_kinds() -> list[str]:
    return sorted(_ACTION_PARAM_SCHEMAS.keys())


def is_ready() -> bool:
    return _STATE.ready


def set_ready(value: bool) -> None:
    _STATE.ready = value
