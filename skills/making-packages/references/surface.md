# Surface authoring reference

Source of truth: the reader in
[surface.rs](../../../crates/lash-kernel-package/src/surface.rs), with build
checks in [build.rs](../../../crates/lash-kernel-package/src/build.rs).
The examples below quote lines verbatim from
[dataframe.surface](../../../examples/dataframe/dataframe.surface),
which [src/lib.rs](../../../examples/dataframe/src/lib.rs)
builds through `Package::build`. They are fragments of that complete example;
members belong under their declaring type and refer to its other declarations.

Use this when writing a surface; the
[package guide](../../../docs/host-packages.md#declare-the-package) explains
the meaning of the declarations.

| Part | Reader rule |
| --- | --- |
| Names | Canonical snake_case segments; package paths join segments with dots. Definition names after `=` are kernel function names. |
| Lines | One declaration per line. Blank lines and lines beginning with `#` after trimming are ignored. |
| Indentation | Any leading whitespace makes a type-member line. It belongs to the preceding top-level type. A new top-level declaration closes that type. |
| Callables | Functions and type members map a signature to a definition with ` = `. Prefix a waiting callable with `async`. |
| Types | A type declares a closed record representation, without the reserved provenance fields. Bare names of the package's types resolve within type expressions. |
| Members | A static has no implicit receiver. Methods have a first slot of their own type. A property has exactly that slot and is synchronous. |
| Slots | Kernel types apply. Optional parameters include absence; an optional record field uses `?`. A rest slot uses `...` and is the last slot, of list type. |
| Operations | `operation` is public; `internal operation` is body-only. Each declares a signature and contract, with optional declared intents. Operations do not map to definitions with `=`. |
| Errors | An error declares its name; the kernel kind is the package path plus that name. |
| Labels | The separate labels file uses a declared member path, a tab and one nonempty title per row. |

A function and an error:

```text
function col(name: Text) -> expr = dataframe.col
error execution_error
```

A type declaration and one of its methods:

```text
type expr = Record{op: Text, args: List(Any), name: Text}
  method neq(self: expr, other: expr) -> expr = dataframe.expr.neq
```

Members of the example's pipeline type show a static, a rest slot and a
waiting method:

```text
  static read_text(text: Text) -> data_quanta = dataframe.data_quanta.read_text
  method sort(self: data_quanta, ...keys: List(expr)) -> data_quanta = dataframe.data_quanta.sort
  async method collect_rows(self: data_quanta) -> List(Any) = dataframe.data_quanta.collect_rows
```

The example's two operations are internal and carry only data:

```text
internal operation run(batch: Json) -> Json contract "example.dataframe.run/1"
internal operation explain(batch: Json) -> Json contract "example.dataframe.explain/1"
```

The separate [transport fixture](../../../crates/lash-kernel-package/src/testing/dataframe.surface)
exercises a public operation, getter property, optional slots and callback-bearing
operation transport. The real example keeps callbacks in the kernel instead.
