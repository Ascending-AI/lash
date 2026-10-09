# Kernel collections

These definitions implement the collection rules below. They are independent
of source dialects. Register the numeric library and the machine functions,
then call `register_collections`. Names resolve to content-addressed identities;
the catalog exposes every body. The bodies are written in kernel text.

- **K-COL-001.** `list.len`, `map.len`, `set.len`, `record.len` and `tuple.len`
  return an integer length and reject any other collection kind. Tuple primitives
  accept `Any` in their signatures because the type grammar has fixed-length
  tuple types; they check for a tuple at runtime.
- **K-COL-002.** `list.get` and `tuple.get` require a numeric integral index
  from zero through length minus one. Negative, fractional, non-finite or
  out-of-range indices raise `index_out_of_range`; a non-number raises
  `type_error`. `at` separately supports negative indices measured from the end.
- **K-COL-003.** `slice` takes integral numeric bounds, translates negative
  bounds from the end and clamps each to `[0, length]`. An omitted end is the
  length. An end before start gives an empty collection. The result is a fresh
  list or immutable tuple holding the same elements.
- **K-COL-004.** `concat` takes two collections of its named sequence kind and
  returns a fresh list or tuple, first one's elements followed by the second's.
- **K-COL-005.** List and tuple `contains` and `index_of` use structural `eq`,
  including exact mixed numbers and unequal NaNs. Index-of returns the first
  position or integer -1. Map and set membership uses key equality, including
  a single NaN key. Invalid keys raise `invalid_key`. A map get on a missing key
  raises `key_missing`; a missing record get returns absent. Record membership
  distinguishes a field containing absent from a missing field.
- **K-COL-006.** Keys, values and entries return fresh lists in iteration order.
  List and tuple keys are integer positions. Map entries are `(key, value)`;
  set entries are `(member, member)`; record entries are `(text field, value)`.
- **K-COL-007.** `copy` is shallow: collections have fresh identity and share
  their contents. `copy_deep` copies nested collections and immutable tuple or
  error contents, preserving sharing within the copied graph. Cycles through
  collection contents raise `cyclic_value`; nesting beyond 128 raises
  `too_deep`. Refs still name their original targets. Closures, task handles,
  function references and host handles retain their opaque identities.
- **K-COL-008.** Mutation helpers have kernel bodies only (`K-LIB-007`). Set,
  insert and remove retain the forms' key and index rules. List push returns
  the new length; pop returns the last element or absent for an empty list;
  remove returns the removed element. List insert accepts an integral index
  from zero through length and moves later elements right. Map/set/record
  remove returns whether it removed an entry. Clear empties the existing
  object, so aliases observe it. Set/insert return the original object.
- **K-COL-009.** Record `with` and `without` return shallow copies with a field
  written or removed. Replacing a field keeps its position, adding appends it,
  and deleting a missing field does nothing. Record mutation uses the forms'
  text index rule, never native mutation.
- **K-COL-010.** `collection.map`, `filter`, `reduce`, `find`, `any`, `all`,
  `for_each` and `group_by` iterate under `K-ITER-001` through `K-ITER-003`,
  calling their function argument as a statement. They take each element before
  the callback, so a wait retains that element. Map collects callback results;
  filter keeps elements whose callback returns true; reduce folds left from
  its required initial value; find returns the first match or absent; any/all
  short-circuit and return false/true on empty input; for-each returns null;
  group-by creates insertion-ordered keys holding lists in visitation order.
  Predicates must return bool and grouping keys must be legal keys.
- **K-COL-011.** `list.sort` is stable and returns the original list. It sorts a
  shallow snapshot using a comparator returning a number: positive moves the
  first element after the second; zero or negative keeps their order. NaN
  raises `unordered`. Only after every comparison succeeds does sort replace
  the list's contents. A throwing comparator leaves the list untouched by sort;
  any mutations the callback itself made remain. A comparator that waits is
  an ordinary activation, retaining its operands and partially sorted snapshot.
- **K-COL-012.** Sum folds numeric addition from integer zero. Zip takes two
  lists and stops at the shorter length. Enumerate pairs each visited element
  with its zero-based integer position. Range takes integer start/end and an
  optional integer step (default 1), excludes end, supports descending steps,
  and raises `zero_step` for zero. All four have kernel bodies only.
- **K-COL-013.** `tasks.unfinished` is the machine's existing definition
  (`K-TASK-021`), not a second native implementation. `tasks.wait_all` joins successive
  snapshots in all mode until none remain, collecting results in snapshot order.
  `tasks.cancel_all` cancels each handle, then joins in all-settled mode, including
  any cleanup waits, repeating for tasks spawned during cleanup. Dialects insert these calls
  before main returns; the kernel's terminal rule remains `K-TASK-018`.
- **K-COL-014.** Every definition carries its charge formula in its identity.
  Reads charge the index/key and lookup sizes; copies and traversal charge
  argument/result sizes; equality scans charge deep contents and each candidate
  comparison; sort charges quadratic list size. Callback work and ordinary
  body forms are charged by the VM as well. Charges do not depend on caches.

This completes the 1.0 baseline; it adds no saved or wire shape. Native functions
read and allocate only. A callback-taking definition never has a native path.

Two small strict utilities complete the building blocks used by these bodies
and the dialects:

- **K-BOOL-001.** `bool.not(x)` takes a bool and returns its inverse. Every
  other kind raises `type_error`; it charges one unit.
- **K-COL-015.** `error.new(kind, message, data?)` constructs an error value
  with text kind/message and the data given, or absent when omitted. It rejects
  non-text kind/message. Its charge covers text sizes and the result's own size;
  it preserves guest identities in data until a boundary copies them.
