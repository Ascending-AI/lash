# Resident tool definition authority

This deterministic runbook validates that every effective resident catalog
entry owns one complete definition and retains an executable route. It also
proves that explicit ambient and restricted access survive durable recovery
without turning a restricted empty set into ambient authority. Read
[`../RULES.md`](../RULES.md) first. The witnesses use only in-memory providers;
the durable phase uses isolated SQLite and PostgreSQL fixtures. No phase makes
a model or external network call.

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
    test(~ambient_and_restricted_empty_select_distinct_resident_catalogs) |
    test(~standard_protocol_distinguishes_ambient_from_restricted_empty_access) |
    test(~rlm_catalog_distinguishes_ambient_from_restricted_empty_access) |
    test(~deferred_call_executes_through_grant_without_mutating_catalog) |
    test(~typescript_deferred_call_executes_through_the_same_grant_path)
  "
' | tee "$LASH_RESIDENT_AUTHORITY_EVIDENCE_DIR/resident-tool-authority.log"
```

Expect exactly twenty-one tests and `21 passed; 0 failed`. The positive witnesses
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

The explicit-access witnesses prove that ambient authority retains captured
residents while restricted empty produces no resident native tools or RLM
documentation. The deferred witnesses start from that restricted empty
catalog: a separately granted tool remains executable in Lashlang and
TypeScript without becoming a resident, re-enumerating the provider, or
consulting a later registry definition.

Abort if the filter runs fewer or more than twenty-one tests, a resolver is called
after catalog construction, any negative case reaches preparation, a same-ID
alias or old request loses its route, or a deferred call is admitted through
resident membership.

## Phase 2 — durable authority bytes

Run the SQLite witness directly. Then run the PostgreSQL witness against a
caller-owned disposable database inside `orb gate`; derive its container name
from `ORB_GATE_ID` and let Docker allocate the host port. Remove the container
on exit. Never point this phase at a shared database.

```bash
orb gate lash "$LASH_RESIDENT_AUTHORITY_FORK" -- bash -lc '
  set -o pipefail
  . ./env.sh
  cargo nextest run -p lash-internal-sqlite-store \
    -E "test(explicit_tool_access_survives_sqlite_recovery_and_invalid_bytes_refuse)"

  container="lash-access-${ORB_GATE_ID//[^[:alnum:]_.-]/-}"
  trap '\''docker rm -f "$container" >/dev/null 2>&1 || true'\'' EXIT
  docker run -d --rm --name "$container" \
    -e POSTGRES_USER=lash -e POSTGRES_PASSWORD=lash -e POSTGRES_DB=lash \
    -p 127.0.0.1::5432 postgres:16-alpine >/dev/null
  until docker exec "$container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    sleep 1
  done
  port="$(docker port "$container" 5432/tcp | sed '\''s/.*://'\'')"
  export LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash"
  export LASH_REQUIRE_POSTGRES=1
  cargo nextest run -p lash-internal-postgres-store \
    -E "test(explicit_tool_access_survives_postgres_recovery_and_invalid_bytes_refuse)"
' | tee "$LASH_RESIDENT_AUTHORITY_EVIDENCE_DIR/tool-access-durable-readback.log"
```

Expect one SQLite test and one PostgreSQL test to pass. Each witness writes
restricted empty authority through the production store, drops and reopens the
store, and reads it through the production recovery path. It then rewrites the
real backend row to prove that the predecessor session-head version and current
missing, null, malformed, unknown, empty-name, duplicate-name, and duplicate-ID
authority bytes refuse. Abort if PostgreSQL reports a skip, either filter runs
zero tests, or any malformed record restores as ambient.

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
| Ambient and restricted empty remain distinct in native and RLM catalogs | explicit-access witnesses retain ambient residents and render no restricted residents | | `resident-tool-authority.log` |
| Restricted empty survives real backend recovery | SQLite and PostgreSQL reopen with restricted mode and zero resident definitions | | `tool-access-durable-readback.log` |
| Historical and malformed access bytes fail closed | predecessor plus current invalid-row cases refuse in both backends | | `tool-access-durable-readback.log` |
