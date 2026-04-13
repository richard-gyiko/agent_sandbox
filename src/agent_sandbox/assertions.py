"""Assertion/action execution API surface for AgentSandbox."""

from __future__ import annotations

from typing import Any

from agent_sandbox import runner


def assert_scenario_expectations(
    scenario: dict[str, Any],
    snapshot: dict[str, Any],
    workflow_metrics: dict[str, Any] | None = None,
    run_metadata: dict[str, Any] | None = None,
) -> None:
    runner.assert_scenario_expectations(
        scenario,
        snapshot,
        workflow_metrics=workflow_metrics,
        run_metadata=run_metadata,
    )


def run_scenario_actions(
    scenario: dict[str, Any],
    snapshot: dict[str, Any],
    workflow_metrics: dict[str, Any] | None = None,
    run_metadata: dict[str, Any] | None = None,
) -> None:
    runner.run_scenario_actions(
        scenario,
        snapshot,
        workflow_metrics=workflow_metrics,
        run_metadata=run_metadata,
    )
