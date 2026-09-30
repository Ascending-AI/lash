# Process Environments Carry Plugin Options, Not Product Metadata

Runtime Process execution environments are typed and closed: they carry execution settings plus plugin-owned process options that an installed plugin can decode to rebuild providers, Tool Catalog entries, Tool Grants, Tool Execution Bindings, and Lashlang Tool Bindings. Submission admits immutable process identity and the closed recorded-input shape; preparation checks the artifact's process and Host Requirements references while deliberately omitting live host-environment validation. The worker reconstructs the plugin environment and validates it against the linked Host Requirements before compilation or any effect. Product-specific durable tool state belongs in immutable snapshots referenced by Process Plugin Options. The host owns those snapshots, authorization and revocation policy.

Permanent reconstruction failures, such as a missing artifact, corrupt payload, or a host-reported permanent snapshot refusal, are process failures. Transient infrastructure failures, such as the remote executor or secret store being temporarily unavailable, remain worker/runtime failures and follow the existing retry/recovery path instead of terminalizing the process as a logical failure.

## Amendment (FIG-4163, 2026-09-30)

Submission and worker validation are separate boundaries; the former submission-time rebuilt-snapshot requirement is superseded by this contract.
[`prepare_lashlang_process_start` and `admit_lashlang_process`](../../crates/lash-lashlang-runtime/src/lib.rs),
[worker validation](../../crates/lash-lashlang-runtime/src/process.rs), and
`process_admission_four_shape_table_preserves_codes_and_prepare_omission` in
[the admission tests](../../crates/lash-lashlang-runtime/src/lib_tests.rs) enforce the distinction.
