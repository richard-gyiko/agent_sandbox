from __future__ import annotations

import json
from pathlib import Path

import yaml
from jsonschema import Draft202012Validator


def _load_json(path: str) -> dict:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def _load_yaml(path: str) -> dict:
    data = yaml.safe_load(Path(path).read_text(encoding="utf-8"))
    assert isinstance(data, dict)
    return data


def test_adapter_contract_resources_mirror_labs_docs() -> None:
    adapter_input_pkg = _load_json(
        "src/agent_sandbox/resources/v3/schema/adapter-input.schema.json"
    )
    adapter_output_pkg = _load_json(
        "src/agent_sandbox/resources/v3/schema/adapter-output.schema.json"
    )
    openapi_pkg = _load_yaml("src/agent_sandbox/resources/v3/openapi/adapter-runner.openapi.yaml")

    assert adapter_input_pkg["$id"] == "https://whizy.dev/labs/v3/adapter-input.schema.json"
    assert adapter_output_pkg["$id"] == "https://whizy.dev/labs/v3/adapter-output.schema.json"
    assert openapi_pkg["openapi"] == "3.1.0"


def test_adapter_contract_v1_invariants() -> None:
    adapter_input = _load_json("src/agent_sandbox/resources/v3/schema/adapter-input.schema.json")
    adapter_output = _load_json("src/agent_sandbox/resources/v3/schema/adapter-output.schema.json")
    openapi_doc = _load_yaml("src/agent_sandbox/resources/v3/openapi/adapter-runner.openapi.yaml")

    assert openapi_doc["info"]["version"] == "1.0.0"
    assert adapter_input["properties"]["schema_version"]["const"] == "lab.adapter.v1"
    assert adapter_output["properties"]["schema_version"]["const"] == "lab.adapter.v1"
    assert adapter_output["properties"]["status"]["enum"] == [
        "COMPLETED",
        "FAILED",
        "TIMEOUT",
        "CANCELLED",
    ]


def test_adapter_contract_fixtures_validate_against_v1_schemas() -> None:
    adapter_input = _load_json("src/agent_sandbox/resources/v3/schema/adapter-input.schema.json")
    adapter_output = _load_json("src/agent_sandbox/resources/v3/schema/adapter-output.schema.json")
    input_fixture = _load_json("tests/fixtures/adapter_sdk/valid_adapter_input.json")
    output_fixture = _load_json("tests/fixtures/adapter_sdk/expected_completed_output.json")

    Draft202012Validator(adapter_input).validate(input_fixture)
    Draft202012Validator(adapter_output).validate(output_fixture)
