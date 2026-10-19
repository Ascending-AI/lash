# lash-dialect-python

A Python front end and exact generated-source codec for the lash kernel.
`lower(source, environment)` turns a subset of Python into a kernel
document; the kernel machine runs it.

Hosts install code and operations as [packages](../../docs/host-packages.md).
Programs use their paths without imports: `dataframe.col(x)`,
`q.reduce_by_key(keys, values)` and `await inbox.send_item(input)`.
Package values expose declared members only; indexing, iterating or
converting one to a dict raises `TypeError`. A changed representation raises
`package.stale_value` at member use and names the artifacts involved.

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

A tool's reply reads as `json.loads` gives it. Its JSON reaches the machine
with every object a kernel record; the helper `py.reply` makes each object,
at every depth, a dict with the object's keys in their order, and keeps
each array a list. So indexing, `in`, `len`, `get`, `keys`, `items`,
`values`, iteration and truthiness behave as on any dict, and an empty
object is false. This holds however the reply is awaited: directly, through
a task or through `asyncio.gather`.

Host bindings such as `history` hold kernel records directly. These support
string-key indexing, `in`, `get(key, default)`, `keys()`, key iteration,
`list(record)`, `len` and truthiness, retaining the record's field order and
nested values. Reading a missing key raises `KeyError` with the key in its
`args`. Record fields are read with subscripts; arbitrary field attributes
are outside the dialect. Records do not offer dict mutation methods,
`items()` or `values()`; tool replies remain ordinary dicts.

Where Python's meaning is the kernel's, the lowerer emits the kernel form:
`==` is `eq`, `is` is `same`, `//` and `%` on integers are `div_floor` and
`rem_floor`, `await tool(x)` is one `perform`, whose reply `py.reply`
reads, an awaited coroutine call is an ordinary call, `asyncio.create_task`
is `spawn`, `await t` on a task is `await t via py.await`, which takes no
turn of its own, and `asyncio.sleep(0)` is `yield`. Where it is not, the lowerer calls a helper: the functions
named `py.*`, written in kernel text under `src/helpers/` and installed
as the `python` package. Truthiness, operators on values whose kind is not
known when lowering, subscripts with negative indices and slices, the
check that a dict or set did not change size under a `for` or a
comprehension, `str`, `repr`
and f-string formatting are helpers.

A source is a cell of a session: the names its top level binds are session
bindings, which the next cell sees, and a cell may `await` at its top
level. In a code mode session, a top-level `def` or lambda is saved as a
self-contained function when the cell succeeds. Captures are frozen then;
later cells copy the definition into their own documents. Captures must be
data or other saved functions; a task capture prevents saving. Saved functions
can seed a new session explicitly, and `await processes.start({"definition":
work, "args": {"n": 21}})` starts one as a durable process when the host offers
the process tools. `await control.finish(value)`, awaited at the top level, answers the turn: it is a
declared control tool, and nothing after it runs (FIG-5781).
Built-in names, including the configured terminal name, cannot be
reused by module bindings (`PY_SHADOWS_BUILTIN`); function locals and
comprehension targets may shadow them. Renaming `len` to `len_`, for
example, leaves the built-in available in later cells. When a cell that made tasks ends, the tasks still running are
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

Missing arguments: a call fills parameters in order and leaves the rest
`absent`, so a function checks its last parameter without a default and
raises `TypeError("f() missing a required positional argument")` in its
body. A `def` in a function's body skips that check when no call can leave
such a parameter unfilled (`scope::always_filled`, FIG-6961): its name is
bound once, by that `def`, and no `global` or `nonlocal` anywhere names it;
and every read of the name, in the body and every scope nested in it, is
the callee of a call that gives each parameter without a default, by
position or by keyword, with no `*` or `**` argument. The argument is about
where the value can go:

- A function's local is visible only to its own body and the scopes nested
  in it, so those reads are every way to reach the value.
- Stored, passed, returned, or read by a closure that hands it on, the value
  leaves the callee position: code the lowerer cannot see may call it with
  any arguments, so it keeps its check.
- Rebound by a second binding, or by a `nonlocal` write from a nested scope,
  the name may hold some other function; every `def` of it keeps its check.
- Recursion and a call from a function defined before the `def` read the
  same binding inside the body, so they are judged like any other call. A
  call made before the name is bound still meets the absent slot and
  `py.name_check`, which this does not touch.
- A nested scope that binds the same name is read as if it were this one;
  that only keeps a check that was not needed.
- No program value is `absent`: a read of a slot that may be unbound goes
  through `py.name_check`. So a call that gives a parameter a value fills
  it.

The check costs a `same(param, absent)` and a branch on every call; a
skipped check changes the program's charge to that of the shorter body.
A module's `def` keeps its check: a later cell, a saved function or a host
may call it with any arguments.

asyncio: `asyncio.sleep`, `asyncio.gather` over coroutine calls, tasks or
one `*tasks`, `asyncio.create_task`, and `asyncio.run` at the top level.
A tool is a package operation in the offered operations map, called by its
canonical path, such as `await web.fetch(x)`.

Packages (`src/lower/packages.rs`): a package is in scope by its path and
nothing is imported. `dataframe.col("x")` calls a function, and
`dataframe.DataQuanta.read_text(t)` a static of a value type, which Python
spells in PascalCase; `getattr(dataframe, "lambda")` reaches a segment
Python cannot write. A call fills the member's slots in order, `absent`
for a slot it leaves out that admits it, and a keyword names one field of
the one options slot that has it: `q.flat_map(f, output="utf8")`. A method
on a value whose package type the front end knows (a member's declared
result) calls that type's member, and a property such as `e.name` reads
through its getter. On a receiver of unknown type the value's brand
selects the member before any argument is evaluated: a package value
takes its type's member before the dialect's method of that name, a
package value without it raises `AttributeError`, and any other value
keeps the dialect's method, so a dict's `count` stays `py.method.count`.
A package value is sealed: subscript, item assignment and deletion,
`len`, iteration, `in`, `list()` and `dict()` raise `TypeError`, a tool's
reply passes one through as it is, and it prints as its type,
`<dataframe.DataQuanta>`. `col = dataframe.col` binds a function value
that calls the member; a namespace, a value type, an error and an
operation are no values. `except dataframe.ExecutionError` catches the
error's kind.

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
(`PY_LITERAL_UNSUPPORTED`). A package namespace, value type or error used
as a value (`PY_PACKAGE_NAMESPACE_VALUE`), an operation read as a value
(`PY_OPERATION_VALUE`) and a member a package or a known package type does
not export (`PY_PACKAGE_MEMBER_UNKNOWN`) are program defects.

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

The first MicroPython `tests/basics` selection lives in `conformance/`.
`manifest.json` pins the upstream revision, each upstream path and source
SHA-256, the MIT notice, and each admitted, refused or excluded disposition.
The original sources are embedded verbatim; `cell` records a reviewed subset
adaptation where needed. A fragment in `path` identifies a separate extract
of `upstream_path`. Unselected upstream files are outside this first corpus.
`recorded.json` is made by this recorder on CPython 3.12, never from upstream
`.exp` files:

```sh
python3 crates/lash-dialect-python/witness/record.py --conformance
python3 crates/lash-dialect-python/witness/record.py --conformance --check --base origin/main
kiln test //crates/lash-dialect-python:lash-dialect-python__unit_test \
  --test_arg=tests::conformance::admitted_basics_keep_cpython_observations_and_dispositions \
  --test_arg=--exact --test_arg=--nocapture
```

`outcomes.json` holds the per-case ratchet separately from admission. The law
executes every admitted cell and tests each refused source's diagnostic;
exclusions retain their reviewed reason. A known failure names its owning
ticket and exact observed record. It may keep that observation or improve to
the CPython answer; every other change fails, including a new refusal.
When a fix lands, promote its outcome to `pass`. The recorder also compares
the outcome ledger against the Git base (default `origin/main`): a recorded
pass cannot fail, become refused or excluded, or leave the corpus, even when
both the admission manifest and outcome ledger change. Passes, failures,
refusals and exclusions are reported separately.

`deviations.md` lists where the dialect is deliberately not Python, with a
law for each entry.

## Exact generated source

`print(document, captions, PrintOptions::default())` emits plain Python with no header
or tables; `read(source, &document, GeneratedSourceLimits::default())`
reconstructs identical canonical document bytes against that base Document,
and the captions the source spells, without authored lowering or library lookup.
Maps are optional and default off. Printing performs no compilation or admission.
The [generic format and paired inventory](../../docs/design/generated-source-python.md)
document indentation, explicit empty blocks, exact literals, aliases and limits.

## Captions

A workflow module spells a step's caption (`K-CAP-001`..`K-CAP-007`) as `#:`
lines directly above it, at its indentation, with no blank line between: the
first line is the title, the rest the description, and a bare `#:` a blank
line in it. Above a statement it captions the statement, above a top-level
`async def` the entry (the workflow's title), and above `else:` (of an `if`,
`for` or `while`), the first `except` or `finally:` that arm; above `elif` it
captions the nested `if`. Above an entry's parameter written on its own line
it captions the parameter, and above a field of a `TypedDict` an entry's
parameter or result is annotated with it captions that field
(`K-CAP-007`), also through `list[...]` and an optional (`| None`) type.
`CAPTIONS` declares every kind and its `guide` says this for a model's
prompt.

A workflow module may declare `TypedDict` classes at its top level: one
`TypedDict` base, no keyword, decorator or type parameter, and a body of
annotated field names (a field may be `NotRequired[...]`), docstrings and
`pass`. Such a class declares a closed record type for entry annotations and
nothing else: it emits no statement, and its name is no value.

The front end decides the owner as it lowers each construct: a control's own
statement, a `def`'s binding, else the statement holding the construct's
first action (a pinned operand's temporary when the first action is pinned),
else the construct's last statement. A predeclared slot and the other
scaffolding never take one. Anything else is a comment, never a refusal: a
`#:` run that is detached, at another indentation, after code, above a
docstring, `pass`, a later `except` or a `try`'s `else:`, or whose text the
caption rule refuses. A docstring is never a caption. Words in `#:` comments
do not count against fresh names, so stripping them keeps the document's
identity. The front end (from the parser's comment tokens) and the
generated-source reader share one `captions` module; a cell has no
captions.
