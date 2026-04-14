"""Built-in assertion handlers for AgentSandbox scenarios."""

from __future__ import annotations

from typing import Any


def assert_workflow_metric_equals(assertion: dict[str, Any], context: Any) -> None:
    if context.workflow_metrics is None:
        raise AssertionError("Workflow metrics were not provided for metric assertion")
    params = assertion.get("params", {})
    key = params["key"]
    value = params["value"]
    assert context.workflow_metrics.get(key) == value, (
        f"Expected workflow metric {key}={value}, got {context.workflow_metrics.get(key)}"
    )


def assert_workflow_session_state_has(assertion: dict[str, Any], context: Any) -> None:
    if context.workflow_metrics is None:
        raise AssertionError("Workflow metrics were not provided for session state assertion")
    params = assertion.get("params", {})
    key = params["key"]
    assert key in context.workflow_metrics, (
        f"Expected workflow session_state to include key '{key}'"
    )


def assert_workflow_session_state_match(assertion: dict[str, Any], context: Any) -> None:
    if context.workflow_metrics is None:
        raise AssertionError("Workflow metrics were not provided for session state assertion")
    params = assertion.get("params", {})
    expected = params.get("values", {})
    if not isinstance(expected, dict):
        raise AssertionError("workflow.session_state_match expects params.values as an object")
    for key, value in expected.items():
        actual = context.workflow_metrics.get(key)
        assert actual == value, f"Expected workflow session_state {key}={value}, got {actual}"


def _event_matches(expected: dict[str, Any], actual: dict[str, Any]) -> bool:
    if actual.get("kind") != expected.get("kind"):
        return False
    expected_attrs = expected.get("attrs", {})
    if not isinstance(expected_attrs, dict):
        return False
    actual_attrs = actual.get("attrs", {})
    if not isinstance(actual_attrs, dict):
        actual_attrs = {}
    return all(actual_attrs.get(key) == value for key, value in expected_attrs.items())


def assert_workflow_event_sequence(assertion: dict[str, Any], context: Any) -> None:
    if context.workflow_metrics is None:
        raise AssertionError("Workflow metrics were not provided for event sequence assertion")
    params = assertion.get("params", {})
    expected = params.get("events", [])
    if not isinstance(expected, list) or not expected:
        raise AssertionError("workflow.event_sequence expects params.events as a non-empty array")
    actual_events = context.workflow_metrics.get("agent_sandbox_events", [])
    if not isinstance(actual_events, list):
        raise AssertionError(
            "workflow.event_sequence expects session_state.agent_sandbox_events list"
        )

    cursor = 0
    for idx, expected_event in enumerate(expected):
        if not isinstance(expected_event, dict):
            raise AssertionError(f"workflow.event_sequence params.events[{idx}] must be an object")
        found = False
        while cursor < len(actual_events):
            actual = actual_events[cursor]
            cursor += 1
            if isinstance(actual, dict) and _event_matches(expected_event, actual):
                found = True
                break
        assert found, (
            f"Expected event sequence item not found: index={idx}, expected={expected_event}, "
            f"actual_events={actual_events}"
        )


def register_default_assertions(
    *,
    register_assertion,
    register_assertion_param_schema,
) -> None:
    register_assertion("workflow.metric_equals", assert_workflow_metric_equals)
    register_assertion_param_schema(
        "workflow.metric_equals",
        "assertions/workflow.metric_equals.params.schema.json",
    )
    register_assertion("workflow.session_state_has", assert_workflow_session_state_has)
    register_assertion_param_schema(
        "workflow.session_state_has",
        "assertions/workflow.session_state_has.params.schema.json",
    )
    register_assertion("workflow.session_state_match", assert_workflow_session_state_match)
    register_assertion_param_schema(
        "workflow.session_state_match",
        "assertions/workflow.session_state_match.params.schema.json",
    )
    register_assertion("workflow.event_sequence", assert_workflow_event_sequence)
    register_assertion_param_schema(
        "workflow.event_sequence",
        "assertions/workflow.event_sequence.params.schema.json",
    )
