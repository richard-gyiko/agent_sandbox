"""Gmail and Drive twin providers for agent-sandbox."""

from agent_sandbox_twins._drive_provider import DriveTwinProvider
from agent_sandbox_twins._gmail_provider import GmailTwinProvider


def register(
    *,
    register_twin_provider,
    register_assertion,
    register_assertion_param_schema,
) -> None:
    """Register all Gmail/Drive providers and assertions."""
    register_twin_provider(GmailTwinProvider())
    register_twin_provider(DriveTwinProvider())

    from agent_sandbox_twins._assertions import register_twin_assertions

    register_twin_assertions(
        register_assertion=register_assertion,
        register_assertion_param_schema=register_assertion_param_schema,
    )


def auto_register() -> None:
    """Auto-register with the agent_sandbox engine."""
    from agent_sandbox.runner import register_assertion, register_assertion_param_schema
    from agent_sandbox.twin_provider import register_twin_provider

    register(
        register_twin_provider=register_twin_provider,
        register_assertion=register_assertion,
        register_assertion_param_schema=register_assertion_param_schema,
    )


auto_register()
