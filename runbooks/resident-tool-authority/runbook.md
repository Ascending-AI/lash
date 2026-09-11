# Resident tool definition authority

This deterministic runbook validates that every effective resident catalog
entry owns one complete definition and retains an executable route. Read
[`../RULES.md`](../RULES.md) first. The witnesses use only in-memory providers;
they make no model, network, or prepared tool call.

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
    test(~dispatch_uses_catalog_pinned_contract_without_reresolution) |
    test(~native_rlm_and_validation_share_one_pinned_definition_under_registry_drift) |
    test(~catalog_pins_contract_once_before_any_projection) |
    test(~missing_contract_is_refused_only_for_effective_members) |
    test(~duplicate_effective_identity_is_refused_before_contract_resolution) |
    test(~replay_reuses_record_without_calling_resolver) |
    test(~deferred_call_executes_through_grant_without_mutating_catalog) |
    test(~typescript_deferred_call_executes_through_the_same_grant_path)
  "
' | tee "$LASH_RESIDENT_AUTHORITY_EVIDENCE_DIR/resident-tool-authority.log"
```

Expect exactly eleven tests and `11 passed; 0 failed`. The positive witnesses
prove that native tool schemas, RLM documentation and host bindings, and
argument validation retain the same catalog-owned contract even when the
source resolver would return a different definition later. A restricted
authority-owned alias with the same `ToolId` also retains the original
registry route.

The negative witnesses prove that an effective member without a contract, an
effective duplicate identity, and a definition without a pinned route are
typed admission failures. The route refusal is checked through both the live
turn pin and the direct plugin-session catalog used by durable process paths.
Their prepare counters remain zero. A malformed manifest removed by catalog
curation does not become an admission failure.

The deferred witnesses prove that replay does not resolve a current contract
and that Lashlang and TypeScript grants continue through the existing deferred
execution path without mutating the resident catalog.

Abort if the filter runs fewer or more than eleven tests, a resolver is called
after catalog construction, any negative case reaches preparation, a same-ID
alias loses its route, or a deferred call is admitted through resident
membership.

## Scorecard

| Claim | Gate | Verdict | Evidence |
| --- | --- | --- | --- |
| One pinned definition drives native and RLM projections and validation | drift witness passes with one resolver call | | `resident-tool-authority.log` |
| Missing contracts and routes fail before preparation | typed negative witnesses pass with zero prepare calls | | `resident-tool-authority.log` |
| Duplicate effective identity is refused before contract lookup | duplicate witness passes with zero resolver calls | | `resident-tool-authority.log` |
| Name curation precedes completeness checks | suppressed malformed nonmember witness passes | | `resident-tool-authority.log` |
| Deferred replay and execution retain their grant authority | three deferred witnesses pass | | `resident-tool-authority.log` |
