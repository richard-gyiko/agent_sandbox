"""Minimal adapter SDK helpers for lab.adapter.v1 request/response handling."""

from __future__ import annotations

import json
import os
from contextlib import contextmanager
from typing import Any


def adapter_failed(
    *,
    code: str,
    message: str,
    status: str = "FAILED",
) -> dict[str, Any]:
    return {
        "schema_version": "lab.adapter.v1",
        "status": status,
        "result": {},
        "metrics": {},
        "artifacts": [],
        "error": {"code": code, "message": message},
    }


def adapter_completed(
    *,
    adapter_name: str,
    run_id: str,
    content: str,
    input_text: str,
    extra_result: dict[str, Any] | None = None,
    metrics: dict[str, Any] | None = None,
) -> dict[str, Any]:
    result = {
        "content": content,
        "input_text": input_text,
    }
    if extra_result:
        result.update(extra_result)
    merged_metrics = {"adapter": adapter_name, "run_id": run_id}
    if metrics:
        merged_metrics.update(metrics)
    return {
        "schema_version": "lab.adapter.v1",
        "status": "COMPLETED",
        "result": result,
        "metrics": merged_metrics,
        "artifacts": [],
        "error": None,
    }


def parse_adapter_input(raw_body: str) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    try:
        payload = json.loads(raw_body) if raw_body else {}
    except json.JSONDecodeError as error:
        return None, adapter_failed(code="invalid_json", message=str(error))

    if payload.get("schema_version") != "lab.adapter.v1":
        return (
            None,
            adapter_failed(
                code="invalid_schema_version",
                message="Expected schema_version=lab.adapter.v1",
            ),
        )

    return payload, None


@contextmanager
def scoped_runtime_env(payload: dict[str, Any]):
    runtime_env = payload.get("runtime", {}).get("env", {})
    if not isinstance(runtime_env, dict):
        runtime_env = {}
    original: dict[str, str | None] = {}
    try:
        for key, value in runtime_env.items():
            env_key = str(key)
            original[env_key] = os.environ.get(env_key)
            os.environ[env_key] = str(value)
        yield
    finally:
        for key, previous in original.items():
            if previous is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = previous
