# lash-dialect-python

A Python front end for the lash kernel. `lower(source, environment)` turns a
subset of Python into a kernel document; the kernel machine runs it. The
crate depends on `lash-kernel-doc` and `lash-kernel-dialect` and on nothing
else of lash, and the kernel was not changed for it: no form, value kind or
library function was added.

## Parser

The parser is `ruff_python_parser` 0.0.11 with `ruff_python_ast` and
`ruff_text_size`, the crates.io releases of the parser the Ruff linter uses.
It parses all of Python 3.13 and recovers nothing: a source that does not
parse is refused as `PY_SYNTAX` with the parser's message and place. No
type of the parser is in this crate's interface. It brings `stacker` and
`psm`, whose build scripts compile a few lines of assembly, and
`unicode_names2`; each has an execution fixup under `tools/buck2/fixups`.

## What runs where

Python values are kernel values: `None` is null, an `int` is a kernel
integer of any size, a `float` a float, a `str` text, a `list`, `dict`,
`set` and `tuple` the kernel's list, map, set and tuple, a function a
closure, a task a task handle and an exception an error value whose kind is
the class name. A dict keyed by tuples is a map keyed by tuples.

Where Python's meaning is the kernel's, the lowerer emits the kernel form:
`==` is `eq`, `is` is `same`, `//` and `%` on integers are `div_floor` and
`rem_floor`, `await tool(x)` is one `perform`, an awaited coroutine call is
an ordinary call, `asyncio.create_task` is `spawn`, and `asyncio.sleep(0)`
is `yield`. Where it is not, the lowerer calls a helper: the functions
named `py.*`, written in kernel text under `src/helpers/` and installed
with `define_helpers`. Truthiness, operators on values whose kind is not
known when lowering, subscripts with negative indices and slices, the
check that a dict or set did not change size under a `for`, `str`, `repr`
and f-string formatting are helpers.

A source is a cell of a session: the names its top level binds are session
bindings, which the next cell sees, and a cell may `await` at its top
level. When a cell that made tasks ends, the tasks still running are
cancelled and waited for with `tasks.cancel_all`, which asks the machine
for them with `tasks.unfinished`; this is what `asyncio.run` does.

## The subset

Statements: assignment to names, subscripts and tuple patterns;
augmented assignment; `if`/`elif`/`else`; `for` and `while` with `else`,
`break` and `continue`; `def` with positional, keyword and default
arguments, and `lambda`; `return`; `global` and `nonlocal`; `try` with
`except`, `else` and `finally`; `raise`, `raise ... from ...` and a bare
`raise`; `assert`; `pass`; `del` of a subscript; `async def`;
`import asyncio`; and `class Name(Base): pass` at the top level to declare
an exception class.

Expressions: integers of any size, floats, strings and f-strings, `None`,
`True` and `False`; lists, dicts, sets and tuples; list, dict and set
comprehensions and generator expressions; subscripts, negative indices
and slices with a step; arithmetic with `+ - * / // % **`; comparisons,
chained too; `in` and `not in`; `is` and `is not`; `and`, `or`, `not`; the
conditional expression; set operators `| & - ^`; calls; `await`;
`type(x).__name__`; an exception's `args` and `__cause__`.

Built-in functions: `print` (with `sep`), `len`, `str`, `repr`, `int`,
`float`, `bool`, `abs`, `min`, `max`, `sum`, `sorted` (with `key` and
`reverse`), `list`, `tuple`, `set`, `dict`, `range`, `enumerate`, `zip`,
`reversed`, `any`, `all`, `round`, `isinstance`, `ord`, `chr`, `divmod`.

Methods: on lists `append`, `extend`, `insert`, `pop`, `remove`, `index`,
`count`, `sort`, `reverse`, `copy`, `clear`; on dicts `get`, `keys`,
`values`, `items`, `setdefault`, `update`, `pop`, `copy`, `clear`; on sets
`add`, `discard`, `remove`, `union`, `intersection`, `difference`, `copy`,
`clear`; on strings `join`, `split`, `strip`, `lstrip`, `rstrip`, `upper`,
`lower`, `startswith`, `endswith`, `replace`, `find`, `count`, `index`; on
tasks `cancel`.

Exceptions: `BaseException`, `Exception`, `ArithmeticError`,
`ZeroDivisionError`, `OverflowError`, `AssertionError`, `LookupError`,
`KeyError`, `IndexError`, `NameError`, `UnboundLocalError`, `RuntimeError`,
`TypeError`, `ValueError`, `asyncio.CancelledError` and the classes a cell
declares from them. A tool's failure is caught by `except Exception`; its
class name is the error's kind.

asyncio: `asyncio.sleep`, `asyncio.gather` over coroutine calls, tasks or
one `*tasks`, `asyncio.create_task`, and `asyncio.run` at the top level.
A tool is a name of `environment.effects`, called as `await tool(x)`.

f-strings: `!r` and `!s`, and specifications of fill, alignment, sign,
`#`, `0`, width, `,` or `_` grouping, precision and one of the codes
`b d e E f F o s x X %`.

## Refusals

Everything else is refused when lowering, with a code of `Code`, the place
and what to write instead: classes other than exception declarations
(`PY_CLASS_UNSUPPORTED`), generators (`PY_GENERATOR_UNSUPPORTED`), `with`
(`PY_WITH_UNSUPPORTED`), `match` (`PY_MATCH_UNSUPPORTED`), decorators
(`PY_DECORATOR_UNSUPPORTED`), imports other than `asyncio`
(`PY_IMPORT_UNSUPPORTED`), `*args`, `**kwargs`, keyword-only and
positional-only parameters and starred targets (`PY_STAR_UNSUPPORTED`), a
keyword argument to a callee no single `def` binds
(`PY_KEYWORD_CALL_DYNAMIC`), bitwise and matrix operators and `%`
formatting of a literal (`PY_OPERATOR_UNSUPPORTED`), attributes and methods
outside the lists above (`PY_ATTRIBUTE_UNSUPPORTED`,
`PY_METHOD_UNSUPPORTED`), a built-in used as a value or one the dialect
lacks (`PY_BUILTIN_AS_VALUE`, `PY_BUILTIN_UNSUPPORTED`), the format codes
`g G n c`, computed specifications, `{x=}` and `!a`
(`PY_FORMAT_SPEC_UNSUPPORTED`), other asyncio functions
(`PY_ASYNC_UNSUPPORTED`), a tool or a coroutine function called without
`await` (`PY_COROUTINE_NOT_AWAITED`), `del` of a name
(`PY_DELETE_UNSUPPORTED`), slice and attribute targets
(`PY_TARGET_UNSUPPORTED`), an exception class used as a value
(`PY_EXCEPTION_CLASS`), complex numbers and bytes
(`PY_LITERAL_UNSUPPORTED`).

Regular expressions are not in the subset: `import re` is refused as
`PY_REGEX_UNSUPPORTED`, and the dialect ships no regex package.

## Proof

Two sets of programs are run on CPython and on the kernel machine, and the
records must be equal.

`witness/captured/<case>.py` is a program for the arc's capture script,
`crates/lash-kernel-conformance/witness/capture.py`, with its tool script
`<case>.tools.json` and the capture `<case>.python.json`: the typed values
it output, the order its tool calls were issued and answered in, and what
it returned or raised. The laws of `src/tests/captured.rs` lower each
program unchanged and run it under the same script. To capture one again,
from the repository root:

```sh
python3 crates/lash-kernel-conformance/witness/capture.py python \
  crates/lash-dialect-python/witness/captured/gather.py \
  crates/lash-dialect-python/witness/captured/gather.tools.json \
  crates/lash-dialect-python/witness/captured/gather.python.json
```

`witness/cases/*.py` are cells, for what that script does not record:
printed text, sleeps, a wait that a cancelled task abandons, the tasks
cancelled when a cell ends, and the park each tool call is issued in.
`witness/record.py` runs each on CPython under a scripted order of
outcomes and writes `witness/recorded.json`; the laws of
`src/tests/witness.rs` require the same record of the kernel machine.
`python3 witness/record.py --check` fails when the record is not what the
installed CPython gives.

Both records were made with CPython 3.12.3.

`deviations.md` lists where the dialect is deliberately not Python, with a
law for each entry.
