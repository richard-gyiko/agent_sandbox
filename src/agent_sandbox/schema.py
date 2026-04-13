"""Schema loading and validation utilities for AgentSandbox DSL."""

from __future__ import annotations

import importlib.resources as pkg_resources
import json
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator
from referencing import Registry, Resource

from agent_sandbox.env import getenv


def read_schema_text(relpath: str) -> str:
    configured_dir = (getenv("AGENT_SANDBOX_SCHEMA_DIR", default="") or "").strip()
    if configured_dir:
        configured_path = Path(configured_dir) / relpath
        if configured_path.exists():
            return configured_path.read_text(encoding="utf-8")

    package_resource = (
        pkg_resources.files("agent_sandbox")
        .joinpath("resources")
        .joinpath("v3")
        .joinpath("schema")
        .joinpath(relpath)
    )
    if package_resource.is_file():
        return package_resource.read_text(encoding="utf-8")

    configured_v3 = (getenv("AGENT_SANDBOX_V3_DIR", default="") or "").strip()
    fallback_root = Path(configured_v3) if configured_v3 else Path.cwd() / "labs" / "v3"
    return (fallback_root / "schema" / relpath).read_text(encoding="utf-8")


def load_schema_json(relpath: str) -> dict[str, Any]:
    return json.loads(read_schema_text(relpath))


def validate_kind_schema(kind: str, data: dict[str, Any]) -> None:
    main_schema = load_schema_json(f"{kind}.schema.json")
    shared_schema = load_schema_json("shared.schema.json")
    shared_uri = "https://whizy.dev/labs/v3/shared.schema.json"
    main_uri = str(main_schema.get("$id", f"https://whizy.dev/labs/v3/{kind}.schema.json"))
    registry = (
        Registry()
        .with_resource(shared_uri, Resource.from_contents(shared_schema))
        .with_resource("shared.schema.json", Resource.from_contents(shared_schema))
        .with_resource(main_uri, Resource.from_contents(main_schema))
    )
    Draft202012Validator(main_schema, registry=registry).validate(data)


def validate_schema_doc(relpath: str, data: dict[str, Any]) -> None:
    schema = load_schema_json(relpath)
    Draft202012Validator(schema).validate(data)
