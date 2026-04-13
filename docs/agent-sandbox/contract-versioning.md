# Adapter Contract Versioning

## Current Public Contract

- Contract name: `lab.adapter.v1`
- Status: frozen for v1 rollout
- Canonical files:
  - `src/agent_sandbox/resources/v3/schema/adapter-input.schema.json`
  - `src/agent_sandbox/resources/v3/schema/adapter-output.schema.json`
  - `src/agent_sandbox/resources/v3/openapi/adapter-runner.openapi.yaml`

## Rules

1. Breaking changes require a new schema version (for example `lab.adapter.v2`).
2. Non-breaking additive changes can stay on v1 only if:
   - existing required fields and enum values are unchanged
   - existing fixture contracts continue to validate
3. Published docs and tests must validate against the packaged resource artifacts.

## CI Enforcement

- `tests/test_adapter_contract_v1.py` enforces:
  - v1 invariants (`schema_version`, status enum, OpenAPI info version)
  - reference fixtures validate against v1 schemas
