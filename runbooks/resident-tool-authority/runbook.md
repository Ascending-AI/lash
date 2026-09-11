# Resident tool definition authority

This deterministic runbook validates that every effective resident catalog
entry owns one complete definition and retains an executable route. Read
[`../RULES.md`](../RULES.md) first. The witnesses use only in-memory providers;
they make no model or network call.

Choose the warm fork and an evidence directory owned by this run. The command
uses `pipefail`, so `tee` cannot hide a failed test process.

```bash
set -o pipefail
: "${LASH_RESIDENT_AUTHORITY_FORK:?set this to the caller-owned warm fork name}"
: "${LASH_RESIDENT_AUTHORITY_EVIDENCE_DIR:?set this to a fresh evidence directory}"
mkdir -p "$LASH_RESIDENT_AUTHORITY_EVIDENCE_DIR"
```

## Phase 1 — deterministic authority witnesses

Do:

```bash
orb gate lash "$LASH_RESIDENT_AUTHORITY_FORK" -- bash -lc '
  . ./env.sh
  heavy-slot cargo nextest run --workspace --locked -E "
    test(~effective_member_without_contract_is_refused_before_prepare) |
    test(~restricted_definition_uses_id_route_and_missing_route_is_refused_before_prepare) |
    test(~plugin_session_refuses_missing_resident_route_before_advertisement) |
    test(~model_request_pin_captures_provider_route_across_same_id_reassignment) |
    test(~dispatch_uses_catalog_pinned_contract_without_reresolution) |
    test(~native_rlm_and_validation_share_one_pinned_definition_under_registry_drift) |
    test(~catalog_pins_contract_once_before_any_projection) |
    test(~missing_contract_is_refused_only_for_effective_members) |
    test(~duplicate_effective_identity_is_refused_before_contract_resolution) |
    test(~replay_reuses_record_without_calling_resolver) |
    test(~execution_grant_routes_multi_provider_source_by_id_not_name) |
    test(~pinned_source_preserves_provider_by_id_overrides) |
    test(~captured_resident_route_does_not_bind_an_unrelated_tool_id) |
    test(~pinned_source_retains_exactly_known_nonadvertised_resident_id) |
    test(~resident_snapshot_refuses_mismatched_known_id_without_overwriting_advertised_route) |
    test(~process_run_context_captures_catalog_and_execution_route_together) |
    test(~deferred_call_executes_through_grant_without_mutating_catalog) |
    test(~typescript_deferred_call_executes_through_the_same_grant_path)
  "
' | tee "$LASH_RESIDENT_AUTHORITY_EVIDENCE_DIR/resident-tool-authority.log"
```

Expect exactly eighteen tests and `18 passed; 0 failed`. The positive witnesses
prove that native tool schemas, RLM documentation and host bindings, and
argument validation retain the same catalog-owned contract even when the
source resolver would return a different definition later. A restricted
authority-owned alias with the same `ToolId` also retains the original
registry route. An actual request pin prepares and executes against provider A
after a later request reassigns the same id to provider B; the attempt-aware
capability and execution paths remain pinned too.
Provider-specific by-ID overrides remain authoritative for ordinary, attempt,
intent, and internal execution, while a captured route never binds an unrelated
tool ID. A process dispatch captures its catalog and registry together, and a
known resident that is resolved exactly by ID remains bound even when it is not
part of the current advertisement.

The negative witnesses prove that an effective member without a contract, an
effective duplicate identity, and a definition without a pinned route are
typed admission failures. The route refusal is checked through both the live
turn pin and the direct plugin-session catalog used by durable process paths.
Their prepare counters remain zero. A malformed manifest removed by catalog
curation does not become an admission failure. A provider that resolves a known
resident ID to a different manifest ID is refused before the source cache or
registry state can change, so it cannot replace another advertised route.

The deferred witnesses prove that replay does not resolve a current contract,
that a hidden provider stays outside the resident snapshot, and that core,
Lashlang, and TypeScript grants continue through the existing deferred
execution path without mutating the resident catalog.

Abort if the filter runs fewer or more than eighteen tests, a resolver is called
after catalog construction, any negative case reaches preparation, a same-ID
alias or old request loses its route, or a deferred call is admitted through
resident membership.

## Scorecard

| Claim | Gate | Verdict | Evidence |
| --- | --- | --- | --- |
| One pinned definition drives native and RLM projections and validation | drift witness passes with one resolver call | | `resident-tool-authority.log` |
| Missing contracts and routes fail before preparation | typed negative witnesses pass with zero prepare calls | | `resident-tool-authority.log` |
| Duplicate effective identity is refused before contract lookup | duplicate witness passes with zero resolver calls | | `resident-tool-authority.log` |
| Name curation precedes completeness checks | suppressed malformed nonmember witness passes | | `resident-tool-authority.log` |
| Resident routes survive same-id provider reassignment | old and fresh requests retain distinct prepare, execute, and attempt routes | | `resident-tool-authority.log` |
| Provider by-ID behavior remains authoritative | ordinary, attempt/intent, and internal override witnesses pass; unrelated IDs ignore the captured name | | `resident-tool-authority.log` |
| Direct process dispatch uses one captured tool surface | old and fresh process contexts retain distinct definitions and routes | | `resident-tool-authority.log` |
| Known nonadvertised residents retain their exact route | restored resident remains curated, nonorphaned, and executable | | `resident-tool-authority.log` |
| Known resident identity mismatches fail atomically | mismatched exact-ID resolution is refused without state or advertised-route changes | | `resident-tool-authority.log` |
| Deferred replay and execution retain their grant authority | four deferred witnesses pass | | `resident-tool-authority.log` |
