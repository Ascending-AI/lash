# r56 probes

Each probe runs one lowered Python program at slices 1, 7 and 50 and pins
its prints, end, effect trace, total charge and the charge at every slice
return. `sort_corpus <root> --core-probes [filter]` runs them on the bytecode
core and `--base-probes` on the base machine.

## Where the literals come from

Prints, end and trace are the base Source executor's output at 58a74c07f5.
Slice returns and charges are derived from that output by hand. Where the
rules say the base is wrong, a literal drops exactly the points the rule
removes, and removes or adds exactly the billing the rule names.

Slice returns:

- **R3** (K-MACH-002, a native Decline makes no step): base points at a
  library body's entry, inside it, or at its exit are dropped. 301 points.
  Since FIG-7689 (7bcba3681b) a body-only library body's statements are
  steps, as authored ones are: the slice-1 literals add the core's 530
  statement-step points in body-only library bodies (`py.mul`,
  `py.print_text`, `py.repr`, `py.add`, `py.for_in.*`, `py.exc.*` and their
  closures), and no other point. A native's alternative body, entered by
  its Decline, still makes none.
- **R2** (K-ITER-005, hooks are library calls): base points at a protocol
  hook, at a loop start or a protocol step, are dropped. 97 points.
- **R4** (K-MACH-002, dispatching a pending return after `finally` makes no
  step): the base point there is dropped. 2 points, in exc2.
- **K-MACH-005** (a slice return leaves tasks ready): where the last ready
  task stops at an action, the run parks; the base's slice return just
  before the park, at the same charge, is dropped. 3 points, in t4.
- Points inside base-only billing (below) are inside a covered extent and
  are dropped. 46 points, in f1 and u1.

The following stay:

- statement starts in uncovered code;
- the end of a stepped frame's last statement, including a task's root
  returning;
- action boundaries, after the action's atoms and before an outcome's
  decode.

Slices 7 and 50 follow from the slice-1 boundaries by the executor's own
rule: return at the first boundary at least a slice past the last return,
counting afresh after a park. This reproduces every base slice-7 and
slice-50 run from its slice-1 run. It reproduces the retargeted slice-7 and
slice-50 literals from the retargeted slice-1 ones the same way: in t2 and
t4 at slice 7 the one boundary just after each park is within a slice of
it and makes no return.

Charges:

| probe | base | literal | why |
|---|---|---|---|
| f1 | 6627 | 6581 | K-CHG-007: the base bills 23 units inside each of two covered `py.getattr` calls on a module attribute, before their `a18` completion formulas |
| u1 | 53550 | 53139 | K-CHG-007: the base bills inside two covered `py.slice` calls: 175 units in `s[::-1]` before its `a64` formula, and 236 units in `(t + "x")[-2:]` before its `a10013` formula |
| t1 | 2181 | 2183 | R5: two spawn atoms inside the covered `py.gather` |
| t2 | 4178 | 4180 | R5: two spawn atoms inside the covered `py.gather` |
| t4 | 12836 | 12839 | R5: three spawn atoms inside the covered `py.gather` |

Every other charge equals the base's.
