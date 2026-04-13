# Twin Workflow Release Gate

This checklist defines minimum scenario coverage before production cutover for
`workflow.email_document_processor` in twin mode.

## Required Green Gates

1. `p0-smoke` runs pass in CI on every PR.
2. `p1-deep` runs pass in scheduled CI (or pre-release pipeline).
3. Deterministic resilience scenarios pass:
   - `workflow_links_download_success_run`
   - `workflow_links_download_failure_run`
   - `workflow_finalize_label_failure_run`

## Coverage Expectations

1. Happy path:
   - fetch + prefilter + collect + finalize with label applied
2. Link pipeline:
   - at least one successful link download
   - at least one failed link download with `failed_downloads > 0`
3. Finalize resilience:
   - at least one forced label failure with `label_errors > 0`
4. Data safety:
   - existing `NOT_RELEVANT` and duplicate scenarios remain green

## CI Recommendation

1. PR pipeline: run `agent-sandbox run execute-tier p0-smoke`.
2. Nightly pipeline: run `agent-sandbox run execute-tier p1-deep`.
3. Block release if any required run regresses or flakes repeatedly.
