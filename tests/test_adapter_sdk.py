from __future__ import annotations

import json
import os
from pathlib import Path

from agent_sandbox.adapter_sdk import (
    adapter_completed,
    adapter_failed,
    parse_adapter_input,
    scoped_runtime_env,
)


def _load_fixture(name: str) -> dict:
    path = Path("tests/fixtures/adapter_sdk") / name
    return json.loads(path.read_text(encoding="utf-8"))


def test_parse_adapter_input_valid_fixture() -> None:
    payload = _load_fixture("valid_adapter_input.json")
    parsed, error = parse_adapter_input(json.dumps(payload))
    assert error is None
    assert parsed is not None
    assert parsed["schema_version"] == "lab.adapter.v1"


def test_parse_adapter_input_rejects_invalid_schema_version_fixture() -> None:
    payload = _load_fixture("invalid_schema_adapter_input.json")
    parsed, error = parse_adapter_input(json.dumps(payload))
    assert parsed is None
    assert error is not None
    assert error["error"]["code"] == "invalid_schema_version"


def test_adapter_completed_matches_fixture() -> None:
    payload = _load_fixture("valid_adapter_input.json")
    expected = _load_fixture("expected_completed_output.json")
    output = adapter_completed(
        adapter_name="python-http-adapter",
        run_id=payload["run"]["id"],
        content=expected["result"]["content"],
        input_text=payload["input"]["text"],
    )
    assert output == expected


def test_adapter_failed_shape() -> None:
    output = adapter_failed(code="x", message="y")
    assert output["schema_version"] == "lab.adapter.v1"
    assert output["status"] == "FAILED"
    assert output["error"] == {"code": "x", "message": "y"}


def test_scoped_runtime_env_applies_and_restores() -> None:
    payload = _load_fixture("valid_adapter_input.json")
    original = os.environ.get("WHIZY_RUNTIME_MODE")
    with scoped_runtime_env(payload):
        assert os.environ.get("WHIZY_RUNTIME_MODE") == "twin"
        assert os.environ.get("WHIZY_TWIN_GMAIL_BASE_URL") == "http://localhost:9200"
    assert os.environ.get("WHIZY_RUNTIME_MODE") == original
