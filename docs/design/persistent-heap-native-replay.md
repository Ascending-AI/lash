# Native replay cutover (FIG-6188)

The probe constructs each reachable fixture through the actual PagedHeap and
Census. Its NativeHeap wrapper delegates every operation and records only calls
to reserve(values, bytes), including whether the delegated call succeeded. It
has no bound or refusal policy. Internal heap reservations are not wrapper calls.
The strict certificate still compares outcomes, normal-key observations, work,
formula charges and reservation fingerprints exactly.

The [entry ledger](persistent-heap-native-replay-ledger.tsv) identifies every
changed record/case, argument seed, object seed, old/new answer and cause. Each
changed entry occurs in exactly one cause class. Counts below distinguish shared
record/case entries from their Python/TypeScript release memberships. All changed
cases have room=0, and all new outcomes are Memory. Positive-room observations
are preserved. The formatter's paged-radix message regression was fixed in code,
not re-recorded; its new law failed once before the fix.

| Cause | Distinct entries | Release memberships |
| --- | ---: | ---: |
| collection_entity_header | 526 | 1002 |
| scalar_materialization | 325 | 629 |
| immutable_range_copy | 31 | 62 |
| Total | 882 | 1693 |

`collection_entity_header`: the old probe's object(Vec) appended an object
without an allocation reservation. The real heap charges the 32-byte entity
header and stored slots. This applies to collection copies/views/record edits,
empty text-to-list results, regex result records, invalid dates and URL result
collections when argument materialization has not already refused. For example,
record 488922b17efd11bfcdce6da6504be15f112e8dca08b74ddeb8e51dffc3a6f99f,
case 6 (set.keys of an empty set) formerly returned an empty list after reserve
(0,0). Allocating that list now refuses its 32-byte header. Work remains 0 and
charge remains 3: statement 1 + empty-set deep size 1 + Absent deep size 1.
A rooted input does not provide free storage for a fresh identity.

`scalar_materialization`: descriptor materialization reserves the temporary
copy's bytes against the real census. Old Arc-backed operands needed no copy
reservation. JSON's text_copy also reserves resident text copied into its own
buffer. Refusal therefore precedes some explicit native reservations, errors
and counted work. For example, record
6f82d9c7f3695fa4586dea6ad2280da112d7be2a32e664607e5a9ed811dcd86e, case 176
(text.trim_set) now refuses the first 16384-byte operand copy before reaching its
old explicit reserve(0,32768). Both outcomes remain Memory, work remains 0 and
formula charge remains 268501005. The wrapper correctly observes no reserve call:
the actual heap's internal call is not duplicated for the oracle.

`immutable_range_copy`: SemanticHeap's text/byte concat and scalar slice reserve
the range they copy, even for a small resident result. The old probe did not
reserve those operations. For example, record
d67a0be67adea0feb20b96a56682821826683f266569115302df98462a27a483, case 61
(text.concat("a","a")) refuses the two-byte result buffer with room=0. Work
remains 0; formula charge drops 8 to 6 because the actual result is Absent
(deep size 1), rather than the two-byte text (deep size 3). This is the unchanged
registered charge formula evaluated on the new bound outcome, not a new formula.

## Invalid synthetic set fixtures

149 shared cases (298 release memberships) were deleted under ruling 338798.
Every deleted row is separately identified in the ledger with its seeds and
K-KEY-001 property. Both illegal object tables were introduced by FIG-6182
(9d1acad3d5); they are signature-edge tables, not release/checkpoint images.
Their rows 85/86/87 are respectively empty, small and large Any-member sets.
Rows 86/87 contain Absent and the large row also contains mutable values.
Each affected record starts with the older sampler's valid 3/4-row tables;
all deleted inputs occur afterward and point to row 86 or 87 in one of:

- 72d2bf8432920e154204e177acbaacfa140273edde27edd89c63b0a298c60e8f (88 rows)
- 5f6722587e087a83ca0806f429d34e20fc29ec7df766ca1ad8c28d6adac947c9 (173 rows)

The old sampler (9d1acad3d5^, build/probe.rs::samples) makes at most a single
Null set member for Any; it cannot make these tables. The signature generator's
empty/small/256-item edge policy accounts for their positions and repetitions.
The source recorder imports fixture Input values, not stored release images.
No production set constructor accepts these members: K-KEY-001 has always
refused them (the old keys.rs::validate explicitly excludes Absent and mutable
values). The replay no longer accepts raw Vec snapshots as a second heap.

The exact surviving refusal oracle is
lash-kernel-vm::laws::forms::operations_raise_typed_errors_outside_their_operand_kinds
(`set{absent}`, `map{[1]:1}`, and a tuple containing a mutable key). Native key
validation is also exercised by
lash-kernel-lib::collections::tests::k_col_005_membership_and_missing_values_are_distinct.
Generator filtering now uses the real heap's normal_key, and every generated or
imported Input is constructed through that heap before it can be recorded.
The new generated_set_edges_are_constructible_by_the_real_heap law exercises
both affected signatures. No valid legacy sampler case was removed.
