"""Built-in assertion handlers for AgentSandbox scenarios."""

from __future__ import annotations

import re
from typing import Any


def _folder_path_by_id(snapshot: dict[str, Any]) -> dict[str, str]:
    drive_folders = snapshot["drive"].get("folders", [])
    folder_path_by_id: dict[str, str] = {}

    folders_by_id = {f["id"]: f for f in drive_folders}
    for folder_id in folders_by_id:
        parts: list[str] = []
        cursor = folder_id
        while cursor and cursor in folders_by_id:
            folder = folders_by_id[cursor]
            name = folder.get("name", "")
            if name and name != "root":
                parts.append(name)
            cursor = folder.get("parent_id")
        folder_path_by_id[folder_id] = "/".join(reversed(parts))
    return folder_path_by_id


def assert_drive_file_exists(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    parent_path = params["parent_path"].strip("/")
    name = params["name"]
    drive_files = context.snapshot["drive"].get("files", [])
    folder_path_by_id = _folder_path_by_id(context.snapshot)
    found = False
    for file_item in drive_files:
        folder_id = file_item.get("parent_id")
        same_path = folder_path_by_id.get(folder_id, "") == parent_path
        same_name = file_item.get("name") == name
        if same_path and same_name:
            found = True
            break
    assert found, f"Expected file '{name}' in '{parent_path}' not found"


def assert_drive_no_file_under_path_prefix(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    path_prefix = str(params["path_prefix"]).strip("/")
    drive_files = context.snapshot["drive"].get("files", [])
    folder_path_by_id = _folder_path_by_id(context.snapshot)

    violating_files: list[str] = []
    for file_item in drive_files:
        folder_id = file_item.get("parent_id")
        parent_path = folder_path_by_id.get(folder_id, "")
        full_path = "/".join(part for part in [parent_path, str(file_item.get("name", ""))] if part)
        if full_path.startswith(path_prefix):
            violating_files.append(full_path)

    assert not violating_files, (
        f"Expected no files under '{path_prefix}', found: {', '.join(violating_files)}"
    )


def assert_drive_has_hash(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    sha256 = params["sha256"]
    drive_files = context.snapshot["drive"].get("files", [])
    found = any(
        file_item.get("app_properties", {}).get("sha256") == sha256 for file_item in drive_files
    )
    assert found, f"Expected hash '{sha256}' not present in drive snapshot"


def assert_gmail_message_has_label(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    message_id = params["message_id"]
    label = params["label"]
    gmail_messages = context.snapshot["gmail"].get("messages", [])
    by_id = {m["id"]: m for m in gmail_messages}
    msg = by_id.get(message_id)
    assert msg is not None, f"Message not found: {message_id}"
    assert label in msg.get("labels", []), f"Expected label '{label}' on message '{message_id}'"


def assert_gmail_op_count(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    action = params["action"]
    expected = int(params["count"])
    status = params.get("status")
    ops = context.snapshot.get("gmail_ops", [])
    matched = 0
    for op in ops:
        if op.get("action") != action:
            continue
        if status and op.get("status") != status:
            continue
        matched += 1
    assert matched == expected, (
        f"Expected gmail op count action={action} count={expected}, got {matched}"
    )


def assert_drive_op_count(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    action = params["action"]
    expected = int(params["count"])
    status = params.get("status")
    ops = context.snapshot.get("drive_ops", [])
    matched = 0
    for op in ops:
        if op.get("action") != action:
            continue
        if status and op.get("status") != status:
            continue
        matched += 1
    assert matched == expected, (
        f"Expected drive op count action={action} count={expected}, got {matched}"
    )


def assert_drive_filename_sequence_valid(assertion: dict[str, Any], context: Any) -> None:
    params = assertion.get("params", {})
    parent_path = str(params["parent_path"]).strip("/")
    pattern_text = str(
        params.get(
            "filename_regex",
            r"^(?P<month>\d{2})-(?P<day>\d{2})-(?P<seq>\d+)-.+\.pdf$",
        )
    )
    pattern = re.compile(pattern_text)
    min_matches = int(params.get("min_matches", 1))
    require_unique = bool(params.get("require_unique_per_day", True))
    require_gapless = bool(params.get("require_gapless_per_day", False))

    drive_files = context.snapshot["drive"].get("files", [])
    folder_path_by_id = _folder_path_by_id(context.snapshot)
    matched_files: list[dict[str, Any]] = []
    by_day: dict[str, list[int]] = {}

    for file_item in drive_files:
        folder_id = file_item.get("parent_id")
        file_parent_path = folder_path_by_id.get(folder_id, "")
        if file_parent_path != parent_path:
            continue
        file_name = str(file_item.get("name", ""))
        match = pattern.match(file_name)
        if not match:
            continue
        matched_files.append(file_item)
        day_key = f"{match.group('month')}-{match.group('day')}"
        seq = int(match.group("seq"))
        by_day.setdefault(day_key, []).append(seq)

    assert len(matched_files) >= min_matches, (
        f"Expected at least {min_matches} files matching sequence pattern "
        f"in '{parent_path}', got {len(matched_files)}"
    )

    for day_key, values in by_day.items():
        if require_unique:
            unique = len(set(values))
            assert unique == len(values), (
                f"Duplicate sequence number detected for day {day_key}: {values}"
            )
        if require_gapless:
            max_seq = max(values)
            expected = list(range(1, max_seq + 1))
            assert sorted(values) == expected, (
                f"Non-gapless sequence for day {day_key}: expected {expected}, got {sorted(values)}"
            )


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
    register_assertion("drive.file_exists", assert_drive_file_exists)
    register_assertion_param_schema(
        "drive.file_exists",
        "assertions/drive.file_exists.params.schema.json",
    )
    register_assertion("drive.no_file_under_path_prefix", assert_drive_no_file_under_path_prefix)
    register_assertion_param_schema(
        "drive.no_file_under_path_prefix",
        "assertions/drive.no_file_under_path_prefix.params.schema.json",
    )
    register_assertion("drive.has_hash", assert_drive_has_hash)
    register_assertion_param_schema(
        "drive.has_hash",
        "assertions/drive.has_hash.params.schema.json",
    )
    register_assertion("drive.op_count", assert_drive_op_count)
    register_assertion_param_schema(
        "drive.op_count",
        "assertions/drive.op_count.params.schema.json",
    )
    register_assertion("drive.filename_sequence_valid", assert_drive_filename_sequence_valid)
    register_assertion_param_schema(
        "drive.filename_sequence_valid",
        "assertions/drive.filename_sequence_valid.params.schema.json",
    )
    register_assertion("gmail.message_has_label", assert_gmail_message_has_label)
    register_assertion_param_schema(
        "gmail.message_has_label",
        "assertions/gmail.message_has_label.params.schema.json",
    )
    register_assertion("gmail.op_count", assert_gmail_op_count)
    register_assertion_param_schema(
        "gmail.op_count",
        "assertions/gmail.op_count.params.schema.json",
    )
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
