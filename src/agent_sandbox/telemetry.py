"""Telemetry helpers and run-context propagation for AgentSandbox."""

from __future__ import annotations

from contextlib import contextmanager
from contextvars import ContextVar
from typing import Any
from uuid import uuid4

_run_ctx: ContextVar[dict[str, Any] | None] = ContextVar(
    "agent_sandbox_run_ctx",
    default=None,
)


def new_run_id() -> str:
    return f"run_{uuid4().hex[:16]}"


@contextmanager
def set_run_context(context: dict[str, Any]):
    token = _run_ctx.set(context)
    try:
        yield
    finally:
        _run_ctx.reset(token)


def get_run_context() -> dict[str, Any]:
    return _run_ctx.get() or {}


@contextmanager
def observed_span(name: str, attributes: dict[str, Any] | None = None):
    merged: dict[str, Any] = {}
    merged.update(get_run_context())
    if attributes:
        merged.update(attributes)

    try:
        from opentelemetry import trace

        tracer = trace.get_tracer("agent_sandbox")
        with tracer.start_as_current_span(name) as span:
            for key, value in merged.items():
                span.set_attribute(str(key), value)
            try:
                yield span
            except Exception as error:
                span.record_exception(error)
                span.set_attribute("error.type", type(error).__name__)
                span.set_attribute("error.message", str(error))
                raise
    except Exception:
        yield None
