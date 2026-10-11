# Saved functions across sessions and cold restore

Read [the shared rules](../RULES.md). This is `deterministic_only`, tier
`deterministic`, model `scripted-provider`, cost $0. It emits no judged row and
adds no workbench save/seed UI. Facade hosts seed functions explicitly through
`RlmSeed.functions`; [ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)
and [the kernel design](../../docs/kernel/design.md) govern their kernel
identities. A saved function must keep its frozen captures and calling
metadata, run after restore, and refuse a missing required effect before code
runs. An earlier helper identity must remain callable while retained.

## Procedure

Source `env.sh`. Run these four existing laws once by full target and selector,
retaining each test log/report and the final receipt. The first three are
Python executor laws on their SQLite-backed durable test host. The last is
the existing SQLite/TypeScript synthetic-next feature witness; the ordinary
kernel-two-build label excludes its laws and must not be used.

```sh
kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=--exact --test_arg=executor::tests::python::a_new_session_created_with_a_saved_function_calls_it
kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=--exact --test_arg=executor::tests::python::a_saved_function_is_called_after_a_cold_restore
kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=--exact --test_arg=executor::tests::python::a_saved_function_is_refused_where_a_tool_it_calls_is_missing
kiln test //crates/lash:kernel_two_build_claim__test__fv_0c6870c9 --test_arg=--exact --test_arg=a_saved_function_holding_an_earlier_helper_identity_is_called_in_a_new_cell
```

Every command must say `kiln: test passed executed=1 passed=1 failed=0`.
Zero execution is Abort, not evidence. A failure is a product finding and the
law stays intact. Keep artifacts under
`.kiln/<ticket>/run-root/saved-functions/` and score the actual outputs against
these decisions, using the source assertions to understand each receipt:

1. **New session:** Python's `greet` lambda is saved with its keyword signature
   and captured `greeting='hello'`; the source state is dropped. A freshly
   seeded state starts with no ordinary bindings, yet calling default and
   named arguments returns `["hello, lash", "hello, world"]`.
2. **Cold restore:** the captured `rate=3` and async `scale(value: int=14)`
   preserve written source and async/default metadata. After graph capture,
   installation and cold restoration, the two calls return `[42, 21]`.
3. **Missing tool:** the saved `shout` requires `echo_say`. Seeding into a
   state without it refuses `PY_SAVED_FUNCTION_UNUSABLE`, naming `shout` and
   `echo_say`; the external tool's answers remain exactly `["a"]`, so the
   refused call produced no second effect. This typed refusal is success for
   the law, not a runbook failure.
4. **Earlier helper:** build N's saved TypeScript `evens` holds its earlier
   `ts.array.forEach` identity. The later helper build's listing names the
   session and saved `evens`; a new cell calls it and finishes with `[2, 4]`.
   This is helper retention evidence, not a sealed release or arbitrary
   pre-1.0 compatibility claim. Never run the helper seal generator here.

## Scorecard

Fill one recorded scorecard from the four receipts. Screenshots are
inapplicable: the subject is the facade/executor law artifacts, not a browser.

| Law/property | Executed | Passed | Expected decision | Observed/verdict/evidence |
| --- | --- | --- | --- | --- |
| New seeded session | | | hello, lash / hello, world; no leaked ordinary bindings | |
| Cold restore | | | 42 / 21; defaults, async metadata and captures retained | |
| Missing effect | | | named PY_SAVED_FUNCTION_UNUSABLE; no second echo | |
| Earlier helper identity | | | listed saved evens; new cell returns 2 / 4 | |

Overall Pass requires four executed and four passed, with each named decision
reached. Record source SHA, target/selector, receipt paths, artifact root and
cost $0. Do not repair a failure by updating the expectation.

[Recorded FIG-5987 scorecard](recorded-run.md).
