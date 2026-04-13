"""Runtime orchestration API surface for AgentSandbox."""

from __future__ import annotations

from typing import Any

from agent_sandbox import runner

TwinEndpoints = runner.TwinEndpoints
ObservabilityStatus = runner.ObservabilityStatus
TwinUnavailableError = runner.TwinUnavailableError


def default_endpoints() -> TwinEndpoints:
    return runner.default_endpoints()


def ensure_twins_available(endpoints: TwinEndpoints, timeout_s: float = 20.0) -> None:
    runner.ensure_twins_available(endpoints=endpoints, timeout_s=timeout_s)


def get_observability_status() -> ObservabilityStatus:
    return runner.get_observability_status()


def run_env_up(compose_file: str = "docker-compose.twins.yml") -> int:
    return runner.run_env_up(compose_file=compose_file)


def run_env_down(compose_file: str = "docker-compose.twins.yml", purge: bool = True) -> int:
    return runner.run_env_down(compose_file=compose_file, purge=purge)


def reset_twins(endpoints: TwinEndpoints) -> None:
    runner.reset_twins(endpoints)


def seed_twins(endpoints: TwinEndpoints, scenario: dict[str, Any]) -> None:
    runner.seed_twins(endpoints=endpoints, scenario=scenario)


def snapshot_twins(endpoints: TwinEndpoints) -> dict[str, Any]:
    return runner.snapshot_twins(endpoints=endpoints)


def run_workflow_for_scenario(
    scenario: dict[str, Any],
    endpoints: TwinEndpoints,
    session_id: str = "agent-sandbox-session",
) -> dict[str, Any]:
    return runner.run_workflow_for_scenario(
        scenario=scenario,
        endpoints=endpoints,
        session_id=session_id,
    )


def run_agent_for_scenario(
    scenario: dict[str, Any],
    endpoints: TwinEndpoints,
    agent_id: str,
) -> dict[str, Any]:
    return runner.run_agent_for_scenario(
        scenario=scenario,
        endpoints=endpoints,
        agent_id=agent_id,
    )
