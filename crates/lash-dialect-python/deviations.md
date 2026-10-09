# Where the dialect is not Python

Each entry is a program of the subset that runs and gives another answer
than CPython 3.12. Each has a law of the same name in
`src/tests/deviations.rs`. A program outside the subset is refused, which
is not a deviation; the README lists those.

| Law | The dialect | CPython | Why |
| --- | --- | --- | --- |
| `bool_is_not_an_integer` | `True == 1` is false, `True + 1` raises `TypeError`, and `True` and `1` are two dict keys. `int(True)`, `isinstance(True, int)` and a bool under a format specification are as in Python. | A bool is the integer 1 or 0. | A kernel bool is not a number (`K-NUM-001`); making it one would put a helper call in every comparison. |
| `annotations_are_trusted` | A parameter or variable annotated `int`, `float` or `str` is lowered to the kernel's operators directly; a value of another kind raises `TypeError` where it is used. | Annotations are not checked. | It is what lets annotated arithmetic run without a helper call. |
| `generator_expression_is_an_eager_list` | `(f(x) for x in xs)` computes every element where it is written and is a list. | The elements are computed as they are asked for. | The kernel has no suspended frame but a task. |
| `is_compares_immutable_values_by_value` | `is` on two equal tuples, strings or integers is true. On lists, dicts, sets and functions it is identity. | Identity, which for equal immutable values depends on the implementation. | `same` is the kernel's identity, and an immutable value has no other (`K-VAL-004`). |
| `range_is_a_list` | `range(n)` is the list of its integers; it prints as one and is built in full. | A lazy sequence. | The kernel has no lazy sequence. A `for` over it behaves the same. |
| `dict_views_are_lists` | `d.keys()`, `d.values()` and `d.items()` are lists made when called. A `for` over one still raises when the dict changes size. | Views that follow the dict. | As above. |
| `a_set_keeps_insertion_order` | A set iterates and prints in insertion order. | In hash order. | A kernel set is ordered (`K-VAL-006`); a program that depends on hash order is not portable Python either. |
| `only_list_plus_equals_mutates_in_place` | `xs += ys` on a list extends it in place. Every other augmented assignment rebinds the name: `s \|= t` and `xs *= 2` leave other names of the old value unchanged. | `\|=`, `&=`, `-=`, `^=` on a set and `*=` on a list change the object. | One in-place form was worth a helper; the others are rare with aliases. |
| `a_coroutine_call_the_front_end_cannot_see_runs_at_once` | Calling an `async def` through a value the front end cannot resolve runs it to completion there and gives its result; awaiting that result gives it again. A call it can resolve is refused without `await`. | A coroutine object that runs when awaited. | An awaited coroutine is an ordinary call; there is no coroutine value. |
| `unbounded_recursion_ends_the_run` | Recursion past the embedder's call depth bound ends the run with the bound's error. | `RecursionError`, which a program can catch. | A bound is the embedder's and no program catches one (`K-BND-001`). |
| `nan_in_a_list_is_not_equal_to_itself` | A list that holds NaN is not equal to itself, and `nan in [nan]` is false. | True, by the identity shortcut. | `eq` has no identity shortcut. |
| `a_float_literal_too_large_does_not_parse` | `float('1e400')` raises `ValueError`. | `inf`. | `float.parse` refuses a literal out of range. |
