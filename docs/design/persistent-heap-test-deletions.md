# Retired flat-heap test machinery (FIG-6188)

The Heap, Room, Key and Value equality fixtures are superseded by the actual
paged heap, real S7 census and descriptor access. Independent recorded corpus
inputs and expected outcomes are preserved. The release probe is test/build
certification machinery; it delegates to the real heap.

| Deleted test | Property law or independent oracle |
| --- | --- |
| `crates/lash-kernel-lib/src/tests.rs::k_val_026_cyclic_equality_uses_active_pairs_and_finite_paths` | corpus_tests::k_lib_009_document_corpus_names_registered_definitions (executes numbers/K-VAL-026.json); paged::laws::heap::cycles_seal_as_a_page_dag |
| `crates/lash-kernel-lib/src/tests.rs::n_pow_reserves_each_product_before_it_multiplies` | memory_laws::integer_amplifiers_refuse_under_the_real_census |
| `crates/lash-kernel-lib/src/tests.rs::an_integer_reserves_its_digits_before_it_writes_them` | memory_laws::integer_amplifiers_refuse_under_the_real_census |
| `crates/lash-kernel-lib/src/text_json/tests.rs::an_amplifier_reserves_its_result_before_it_builds_it` | memory_laws::text_and_json_amplifiers_refuse_under_the_real_census |
| `crates/lash-kernel-lib/src/text_json/tests.rs::a_refused_json_tree_still_reports_the_texts_own_error` | text_json::laws::a_refused_json_tree_preserves_its_syntax_and_depth_errors |
| `prepared_tests::prepared_entries_answer_as_the_general_call` | Each independent numeric corpus case checks its prepared answer; strict release native replay compares prepared fingerprints. |
| `collections::tests::k_bnd_003_collection_expansion_reserves_before_building_results` | `collections::tests::collection_expansion_refuses_under_the_real_census` |
| `lash-ext-regex-ecma::tests::a_result_the_heap_has_no_room_for_is_refused_at_its_reservation` | `tests::a_regex_expansion_refuses_under_the_real_census` uses the real S7 census; regex semantic and cold/warm laws retain their existing expectations. |

| `crates/lash-dialect-python/src/tests/machine.rs::arithmetic_successors_and_held_previous_definitions_resume_exactly` | `tests::machine::paged_arithmetic_keeps_held_definitions_and_exact_charge` executes sealed S4 checkpoints with the real S7 census; old whole-state JSON export/import oracle removed |
| `crates/lash-dialect-python/src/tests/machine.rs::integer_facts_resume_a_parked_loop_exactly` | `tests::machine::paged_integer_loop_keeps_facts_across_await` checks compiler facts and exact effects through sealed S4 checkpoints |

Conformance's `check_migration`/`MigrationCheck` and synthetic-next feature
membership are deleted under owner ruling 337494. The integrator re-expresses
release-change survival at S8 `classify_carry`/`adopt`. Document/definition
version refusals and all recorded observations retain their original oracles.

| `crates/lash-kernel-conformance/src/migration_laws.rs::a_run_parked_at_any_park_resumes_under_the_next_kernel_version_as_it_would_have_run` | integrator-owned S8 law: parked run survival across release changes (owner ruling 337494); whole-state ParkedRun subject deleted |

| `crates/lash-kernel-conformance/src/migration_laws.rs::a_run_parked_in_a_library_body_is_refused_with_the_site_it_stands_on` | integrator-owned S8 law: unmappable old runtime coordinates are refused (owner ruling 337494); whole-state ParkedRun subject deleted |

| `crates/lash-kernel-conformance/src/migration_laws.rs::a_function_saved_under_one_version_is_migrated_and_called_under_the_next` | integrator-owned S8 law: saved captures survive a release change (owner ruling 337494); whole-state ParkedRun subject deleted |

| `crates/lash-dialect-typescript/src/tests/machine.rs::numeric_entry_saved_functions_accept_later_unknown_calls` | `saved::laws` uses frozen capture bundles; `numeric_recursive_calls_admit_once_and_account_real_work` pins generic numeric entry behavior; old SavedFunction JSON grammar removed |

| `crates/lash-dialect-typescript/src/tests/positional.rs::a_positional_body_parked_at_sixty_recursive_calls_round_trips_exactly` | `a_paged_positional_stack_resumes_with_a_new_layout` reads the S4 frame tree's 62-frame summary and resumes in Layout(17); old flat JSON roundtrip assertion deleted |


| `lash-kernel-doc::graph_codec::tests::value_and_datum_variants_carry_their_pinned_numbers` (flat Value portion) | Datum discriminants remain in `datum_variants_carry_their_pinned_numbers`; semantic descriptors use the canonical heap/root laws in `paged::laws::heap` and `paged::laws::root`. |
| `lash-kernel-doc::graph_codec::tests::documents_and_definitions_read_back_exactly` (flat Object portion) | Documents, definitions and annotations remain; map order survives seal/open in `paged::laws::heap::map_iteration_survives_removal_and_reinsertion_across_seal`. |
