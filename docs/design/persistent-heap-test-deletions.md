# Retired flat-heap test machinery (FIG-6188)

The Heap, Room, Key and Value equality fixtures are superseded by the actual
paged heap, real S7 census and descriptor access. Independent recorded corpus
inputs and expected outcomes are preserved. The release probe is test/build
certification machinery; it delegates to the real heap.

| Deleted test | Property law or independent oracle |
| --- | --- |
| `crates/lash-kernel-lib/src/tests.rs::k_num_001_integer_arithmetic_is_arbitrary_precision` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_001_integer_arithmetic_is_exact_across_the_64_bit_word` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_002_binary64_rounds_ties_to_even` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_003_mixed_arithmetic_converts_once_and_refuses_overflow` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_004_integer_division_rounds_the_exact_quotient_once` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_005_floor_division_remainder_has_divisors_sign` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_006_truncating_remainder_has_dividends_sign` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_007_float_remainders_and_zero_division_are_explicit` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_num_008_arithmetic_never_coerces` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_020_cross_kind_values_are_unequal_without_coercion` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_021_numbers_compare_exactly_even_beyond_float_range` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_022_scalar_contents_define_equality` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_023_tuples_compare_members_and_nan_stays_unequal` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_024_container_contents_ignore_map_set_and_record_order` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_025_errors_functions_handles_closures_tasks_and_refs` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_026_cyclic_equality_uses_active_pairs_and_finite_paths` | corpus_tests::k_lib_009_document_corpus_names_registered_definitions (executes numbers/K-VAL-026.json); paged::laws::heap::cycles_seal_as_a_page_dag |
| `crates/lash-kernel-lib/src/tests.rs::k_val_027_same_is_datum_or_object_identity` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_028_equal_numeric_keys_share_hashes` | strict release corpus key certification through NativeHeap::normal_key; tests::native_corpus_heap_obeys_kernel_key_equality |
| `crates/lash-kernel-lib/src/tests.rs::k_val_029_order_is_exact_and_lexicographic_with_prefix_first` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_030_text_orders_by_unicode_scalar_value` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_031_float_text_is_shortest_round_trip_with_pinned_layout` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_032_integer_text_is_canonical_decimal` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_val_033_nan_is_unordered_for_all_predicates` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_key_001_only_immutable_keys_and_refs_are_admitted` | corpus_tests::k_lib_009_document_corpus_names_registered_definitions; strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_key_002_equal_numbers_nan_and_mixed_tuples_are_one_key` | corpus_tests::k_lib_009_document_corpus_names_registered_definitions; strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_key_004_ref_takes_identity_and_rejects_immutable_values` | corpus_tests::k_lib_009_document_corpus_names_registered_definitions; strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::n_pow_nonnegative_integer_exponents_are_exact_and_guarded` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::n_pow_reserves_each_product_before_it_multiplies` | memory_laws::integer_amplifiers_refuse_under_the_real_census |
| `crates/lash-kernel-lib/src/tests.rs::an_integer_reserves_its_digits_before_it_writes_them` | memory_laws::integer_amplifiers_refuse_under_the_real_census |
| `crates/lash-kernel-lib/src/tests.rs::n_abs_neg_and_min_max_preserve_pinned_zero_and_nan_edges` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::n_round_modes_keep_kinds_nonfinite_values_and_signed_zero` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::n_conversions_require_finite_integral_values_and_exact_range` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::n_parse_and_radix_format_are_explicit_and_noncoercing` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/tests.rs::k_fn_007_every_strict_function_checks_every_operand_kind` | corpus_tests::k_lib_006_recorded_native_edges_and_charges_match (424 independent numeric cases); strict release native corpus |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_val_007_astral_length_and_index_have_distinct_addressing` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_val_007_utf16_slice_refuses_split_pair_boundaries` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ltxt_003_search_reports_the_selected_address_space` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_val_030_code_point_order_differs_from_unit_order` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ltxt_005_literal_split_join_replace_and_concat` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ltxt_006_case_and_whitespace_are_versioned_and_locale_free` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ltxt_007_repeat_and_padding_count_scalars` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ltxt_008_prefix_suffix_and_empty_patterns` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_val_006_scalar_and_utf16_conversion_refuse_lone_surrogates` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_val_008_octets_slice_concat_and_compare` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_lbytes_002_utf8_is_strict_without_replacement` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_eff_005_json_integer_past_u64_round_trips_without_float` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_eff_005_exponents_decode_under_each_stated_type` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_eff_006_bare_numbers_follow_the_document_policy` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_eff_007_fractional_int_overflow_wrong_kind_and_union` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ljson_001_json_syntax_and_surrogate_pairs_are_strict` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_eff_003_json_collections_are_fresh_and_ordered` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_eff_007_json_structured_types_are_checked_before_allocation` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ljson_004_stringify_refuses_absent_nonfinite_keys_and_handles` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ljson_005_cycles_are_refused_but_sharing_is_copied` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ljson_008_direct_render_bounds_and_preserves_decimal_policy` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ljson_006_native_parse_applies_explicit_number_kind_recursively` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_lfmt_001_precision_is_explicit_and_preserves_exact_integers` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_lfmt_003_decimal_parts_preserve_specials_and_binary64_edges` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_lfmt_003_shortest_digits_are_bounded_charged_and_reserved` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_lfmt_002_radix_and_padding_are_language_neutral` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_val_001_native_domains_never_coerce` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_lib_006_native_corpus_pins_results_charges_and_determinism` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/tests.rs::an_amplifier_reserves_its_result_before_it_builds_it` | memory_laws::text_and_json_amplifiers_refuse_under_the_real_census |
| `crates/lash-kernel-lib/src/text_json/tests.rs::a_refused_json_tree_still_reports_the_texts_own_error` | text_json::laws::a_refused_json_tree_preserves_its_syntax_and_depth_errors |
| `crates/lash-kernel-lib/src/text_json/tests.rs::classification_prices_the_actual_walk_and_stops_at_the_first_refusal` | text_json::laws::classification_needs_no_extra_guest_space; strict release native corpus |
| `crates/lash-kernel-lib/src/text_json/tests.rs::k_ltxt_012_join_admission_bounds_scan_copy_and_first_mismatch` | strict release native corpus (tests::native_corpus_certifies_answers_and_keeps_compatible_anchors) |
| `crates/lash-kernel-lib/src/text_json/trim_tests.rs::k_ltxt_011_trim_set_bounds_scan_copy_and_reserves_output` | text_json::laws::trimming_uses_the_supplied_scalar_set_and_ends; strict release native corpus |
| `prepared_tests::prepared_entries_answer_as_the_general_call` | Each independent numeric corpus case checks its prepared answer; strict release native replay compares prepared fingerprints. |
| `collections::tests::k_bnd_003_collection_expansion_reserves_before_building_results` | `collections::tests::collection_expansion_refuses_under_the_real_census` |
| Flat-heap charge and Room assertions in collection range/key-identity tests | The VM-level numeric-bound and identity laws remain; strict release native corpus certifies exact charges and reserve observations. |
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
