# Open ingress scan inventory (FIG-4033)

The query text lives in three store SQL modules. `lash-store-sql` renders its
shared statements for both dialects; each backend also has local statements.
The columns below name the statement and the file containing its query text.

| Table and scan | Query text |
| --- | --- |
| Inputs: `list_undelivered`, `earliest_next_turn_candidate_seq` | `crates/lash-store-sql/src/turn_ingress/pending_inputs.rs` |
| Inputs: `has_claimable_work`, `pending_session_work_ordering` | `crates/lash-store-sql/src/turn_ingress.rs` |
| Inputs: `select_pending_active`, `admission_candidates_next_turn`, `admission_candidates_active_turn_after_work`, `admission_candidates_active_turn_before_completion` | `crates/lash-sqlite-store/src/turn_ingress/pending_inputs.rs`; `crates/lash-postgres-store/src/postgres/turn_ingress/pending_inputs.rs` |
| Inputs at checkpoints: `checkpoint_work_pending_after_work`, `checkpoint_work_pending_before_completion` | `crates/lash-sqlite-store/src/turn_ingress/family.rs`; `crates/lash-postgres-store/src/postgres/turn_ingress/family.rs` |
| Queued work: `list_open` | `crates/lash-store-sql/src/turn_ingress/queued_batches.rs` |
| Queued work: `admission_candidates_idle`, `admission_candidates_turn_lane`, `admission_candidates_boundary` | `crates/lash-sqlite-store/src/turn_ingress/queued_work.rs`; `crates/lash-postgres-store/src/postgres/turn_ingress/queued_work.rs` |

The input state index must contain every undelivered state row, including a row
bound to a root. `list_undelivered` has no `admitted_root IS NULL` predicate.
Both stores' `list_pending_turn_inputs` methods execute that statement.
The queued-work admission index starts with `(session_id, admitted_root)`;
settled batches are deleted, while an admitted batch stays until settlement.
