from __future__ import annotations

from agent_sandbox.target_sdk import (
    capture_events,
    current_events,
    emit_event,
    load_target_runtime_config,
)


def test_target_sdk_defaults_to_live() -> None:
    cfg = load_target_runtime_config(env={})
    assert cfg.mode == "live"
    assert cfg.use_twin is False
    assert cfg.twin_gmail_base_url == "http://localhost:9200"
    assert cfg.twin_drive_base_url == "http://localhost:9100"


def test_target_sdk_twin_mode_and_urls() -> None:
    cfg = load_target_runtime_config(
        env={
            "WHIZY_RUNTIME_MODE": "twin",
            "WHIZY_TWIN_GMAIL_BASE_URL": "http://gmail-twin:9200",
            "WHIZY_TWIN_DRIVE_BASE_URL": "http://drive-twin:9100",
        }
    )
    assert cfg.mode == "twin"
    assert cfg.use_twin is True
    assert cfg.twin_gmail_base_url == "http://gmail-twin:9200"
    assert cfg.twin_drive_base_url == "http://drive-twin:9100"


def test_target_sdk_unknown_mode_normalizes_to_live() -> None:
    cfg = load_target_runtime_config(env={"WHIZY_RUNTIME_MODE": "google"})
    assert cfg.mode == "live"


def test_target_sdk_event_capture_collects_events() -> None:
    with capture_events() as events:
        emit_event("workflow.started", run_id="run-1")
        emit_event("workflow.completed")
    assert events == [
        {"kind": "workflow.started", "attrs": {"run_id": "run-1"}},
        {"kind": "workflow.completed"},
    ]


def test_target_sdk_event_capture_is_scoped() -> None:
    emit_event("outside.scope")
    assert current_events() == []
