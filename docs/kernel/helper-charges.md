# Library helper charge schedule

Every library call bills its completion formula under K-CHG-003/007.
Bodies and library calls within their synchronous extent add no form or
completion charges; spawned library task roots bill their own formulas;
callbacks and closures remain ordinary charged code. A loop whose work
cannot be bounded by argument/result sizes declares `metered 2`: two
units per continuation test, including the terminating test. Those events
also fire in nested library bodies. Every plan must emit the same events.

The audit covers 612 definitions: 120 Python and 492 TypeScript. The table
records changed formulas and metered loops. Unchanged definitions retain
fixed dispatch/table costs, bounded binary64 algorithms or existing size
formulas. Sizes read the argument values at completion; a traversal that
shrinks its controlling input therefore needs metered events.

Hot graph-size schedules: `ts.to_primitive`, `ts.to_number`, `ts.to_string`,
`ts.to_property_key`, `ts.add/sub/mul/div/rem/pow`, `ts.neg`,
`ts.loose_equals/loose_not_equals`, `ts.relational`, `ts.lt/gt/le/ge`,
`ts.array.integer` need deep sizes for collection coercion. Python
`py.contains` and `py.getitem/setitem/delitem` measure structural keys.
Truth tests, `ts.get`, numeric indexes and iterator steps retain shallow
or constant schedules.

| Helper | Previous formula | Completion formula | Metered loops | Reason |
|---|---|---|---|---|
| `py.number_underscores` | `sum(8, size(digits))` | `sum(8, product(size(digits), size(digits)))` | 0 × 2/test | Repeated text assembly copies growing prefixes. |
| `py.mul` | `sum(4, size(left), size(right), size(result))` | `sum(4, size(left), size(right), size(result))` | 2 × 2/test | Empty sequences repeat without growing; numeric multiplication keeps its size schedule. |
| `py.format.group` | `sum(4, size(digits))` | `sum(4, product(size(digits), size(digits)))` | 1 × 2/test | Repeated prefix copies are quadratic; zero width requires a metered termination guard. |
| `py.sorted` | `sum(8, product(256, size(values), sum(1, ceil_log2(size(values)))))` | `sum(8, product(2, size(values)), size(result))` | 0 × 2/test | Linear copying and key collection; key callbacks and the native merge bill separately. |
| `py.method.sort` | `sum(8, product(256, size(self), sum(1, ceil_log2(size(self)))))` | `sum(8, product(4, size(self)))` | 1 × 2/test | Linear copying, key collection, clearing and replacement; the native merge bills separately, and replacement pays 2 per test. |
| `ts.array.integer` | `8` | `sum(8, deep(value))` | 0 × 2/test | Numeric coercion parses variable-sized input. |
| `ts.array.dense` | `8` | `sum(8, size(this), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.spread` | `8` | `sum(8, size(this), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.construct` | `8` | `sum(8, deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.of` | `8` | `sum(8, deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.push` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.slice_generic` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.slice` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.concat` | `8` | `sum(8, deep(this), deep(args), size(result))` | 0 × 2/test | Nested input arrays are traversed when concatenating. |
| `ts.array.fill` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.copyWithin` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.searchable` | `1` | `sum(1, size(this))` | 0 × 2/test | Admission visits every receiver slot. |
| `ts.array.search` | `1` | `sum(1, size(this), deep(args))` | 0 × 2/test | Range search compares candidates with the search operand. |
| `ts.array.generic_indexOf` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.indexOf` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.generic_lastIndexOf` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.lastIndexOf` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.generic_includes` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.includes` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.map` | `8` | `sum(8, size(this), deep(args), size(result))` | 3 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.filter` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.forEach` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.every` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.some` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.find` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.findIndex` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.findLast` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.findLastIndex` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.reduce` | `8` | `sum(8, size(this), deep(args), size(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.reduceRight` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.generic_join` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.join` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.toString` | `8` | `sum(8, size(this), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.reverse` | `8` | `sum(8, size(this), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.toReversed` | `8` | `sum(8, size(this), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.splice` | `8` | `sum(8, size(this), deep(args), size(result))` | 4 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.shift` | `8` | `sum(8, size(this), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.unshift` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.toSpliced` | `8` | `sum(8, size(this), deep(args), size(result))` | 3 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.with` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.array.sort` | `8` | `sum(8, size(this), deep(args), size(result))` | 6 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. A comparator sort runs `list.sort`'s merge in the body itself, one metered loop over each pass placing one element per test, and the comparator is its only callback (FIG-6886). |
| `ts.array.toSorted` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.flatten` | `8` | `sum(8, size(source), size(depth), size(output), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.flat` | `8` | `sum(8, size(this), deep(args), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.flatMap` | `8` | `sum(8, size(this), deep(args), size(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.for_of.open` | `8` | `sum(8, size(subject), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.for_of.next` | `8` | `sum(8, size(driver), size(result))` | 0 × 2/test | Container copying, scans and delegated collection work scale with operands and output. |
| `ts.array.from` | `8` | `sum(8, deep(args), size(result))` | 3 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.boolean.convert` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.to_string` | `8` | `sum(8, deep(value), size(result))` | 0 × 2/test | Coercion traverses collections even on raise. |
| `ts.to_property_key` | `8` | `sum(8, deep(key), size(result))` | 0 × 2/test | Primitive conversion and stringification traverse the operand. |
| `ts.iterate` | `sum(4, size(target))` | `sum(4, size(target))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set` | `sum(12, deep(key))` | `sum(12, deep(key))` | 3 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.keys` | `sum(4, size(target))` | `sum(4, product(size(target), size(target)))` | 0 × 2/test | Numeric record keys use insertion sorting. |
| `ts.assign` | `sum(4, size(source))` | `sum(4, product(size(source), size(source)))` | 0 × 2/test | Delegated source key ordering is quadratic. |
| `ts.object_rest` | `sum(4, size(source), size(taken))` | `sum(4, product(size(source), size(source)), size(taken))` | 0 × 2/test | Source key ordering is quadratic before exclusions. |
| `ts.boundary` | `sum(4, deep(value))` | `sum(4, deep(value))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.date.utc` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.construct` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.parse` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.UTC` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.parts` | `sum(8, deep(result))` | `sum(8, deep(date), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getTime` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.valueOf` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCFullYear` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCMonth` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCDate` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCDay` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCHours` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCMinutes` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCSeconds` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getUTCMilliseconds` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.toISOString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.toUTCString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.toString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.toJSON` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getFullYear` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getMonth` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getDate` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getDay` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getHours` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getMinutes` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getSeconds` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.getMilliseconds` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setTime` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setFullYear` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setMonth` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setDate` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setHours` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setMinutes` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setSeconds` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.date.setMilliseconds` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.name` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.construct` | `sum(8, deep(result))` | `sum(8, size(name), deep(args), size(aggregate), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_Error` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_TypeError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_TypeError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_RangeError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_RangeError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_SyntaxError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_SyntaxError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_ReferenceError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_ReferenceError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_URIError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_URIError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_EvalError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_EvalError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.new_AggregateError` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.is_AggregateError` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.error.toString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.function.call` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.function.apply` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.function.bind` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.function.toString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.json.parse` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.json.stringify_generic` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.json.stringify` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.map.construct` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.map.clear` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.map.keys` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.map.values` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.map.entries` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.map.forEach` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.map.groupBy` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.abs` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.floor` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.ceil` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.trunc` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.sign` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.acos` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.asin` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.acosh` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.asinh` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.atan` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.atanh` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.cbrt` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.cos` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.cosh` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.exp` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.expm1` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.log` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.log1p` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.log10` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.log2` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.sin` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.sinh` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.tan` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.tanh` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.sqrt` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.round` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.atan2` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.pow` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.max` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.min` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.hypot` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.imul` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.clz32` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.math.fround` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.convert` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.isFinite` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.isNaN` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.isInteger` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.isSafeInteger` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.parseFloat` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.parseInt` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.valueOf` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.toString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.toFixed` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.toExponential` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.toPrecision` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.global_isFinite` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.global_isNaN` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.digit` | `sum(8, deep(result))` | `sum(8, size(character), size(radix), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.parts` | `sum(8, deep(result))` | `sum(8, size(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.layout` | `sum(8, deep(result))` | `sum(8, size(digits), size(point), size(scientific), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.number.to_string` | `sum(8, deep(result))` | `sum(8, size(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.convert` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.keys` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.values` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.entries` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.fromEntries` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.assign` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.hasOwn` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.is` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.object.groupBy` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.promise.members` | `sum(8, size(items))` | `sum(8, size(items), size(result))` | 0 × 2/test | A materialized iterator can produce more members than its descriptor size. |
| `ts.promise.all` | `48` | `sum(48, deep(args))` | 0 × 2/test | Eager member/reaction preparation scales with the iterable input. |
| `ts.promise.all_settled` | `48` | `sum(48, deep(args))` | 0 × 2/test | Eager member/reaction preparation scales with the iterable input. |
| `ts.promise.reactions` | `sum(8, size(items))` | `sum(8, size(items), size(result))` | 0 × 2/test | A materialized iterator can produce more members than its descriptor size. |
| `ts.promise.race` | `48` | `sum(48, deep(args))` | 0 × 2/test | Eager member/reaction preparation scales with the iterable input. |
| `ts.promise.any` | `48` | `sum(48, deep(args))` | 0 × 2/test | Eager member/reaction preparation scales with the iterable input. |
| `ts.regexp.require` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.create` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.call` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.snapshot` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.match_record` | `sum(8, deep(result))` | `sum(8, deep(match), size(input), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.exec` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.test` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.global` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.ignoreCase` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.multiline` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.dotAll` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.unicode` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.sticky` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.flags` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.source` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.toString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.regexp.argument` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.match` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.search` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.matchAll` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.split_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.split` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.substitute` | `sum(8, deep(result))` | `sum(8, size(replacement), size(text), size(needle), size(position), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.replace_value` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), size(replace_every), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.replace` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.replaceAll` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.construct` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.add` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.has` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.delete` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.clear` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.values` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.entries` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.set.forEach` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.union` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.intersection` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.difference` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.symmetricDifference` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 2 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.isSubsetOf` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.isSupersetOf` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.set.isDisjointFrom` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.string.integer` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.relative` | `sum(8, deep(result))` | `sum(8, deep(value), size(length), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.length_value` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.uint32` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.receiver` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.slice_units` | `sum(8, deep(result))` | `sum(8, size(text), size(start), size(end), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.create` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.length` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.at_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.at` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.charAt` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.charCodeAt` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.codePointAt` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.slice_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.slice` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.substring_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.substring` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.indexOf_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.indexOf` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.lastIndexOf` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.includes_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.startsWith_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.endsWith_generic` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.includes` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.startsWith` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.endsWith` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.concat` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.normalize` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.toLowerCase` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.toUpperCase` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.trim` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.trimStart` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.trimEnd` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.repeat` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.padStart` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.padEnd` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.fromCharCode` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.fromCodePoint` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.string.raw` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.string.apply_arguments` | `sum(8, deep(result))` | `sum(8, deep(value), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.uri.escape` | `sum(8, deep(result))` | `sum(8, size(octet), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.uri.encode` | `sum(8, deep(result))` | `sum(8, size(input), size(preserve), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.uri.decode` | `sum(8, deep(result))` | `sum(8, size(input), size(preserve), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.uri.encodeURI` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.uri.decodeURI` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.uri.encodeURIComponent` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.uri.decodeURIComponent` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url.construct` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url.canParse` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.refresh` | `sum(8, deep(result))` | `sum(8, deep(params), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.construct` | `sum(8, deep(result))` | `sum(8, deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.toString` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.get` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.getAll` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.has` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.append` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.set` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.url_search_params.delete` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 1 × 2/test | Dynamic or shrinking traversal pays 2 per test; formula covers the remaining work. |
| `ts.url_search_params.sort` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.keys` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.values` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.entries` | `sum(8, deep(result))` | `sum(8, deep(this), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |
| `ts.url_search_params.forEach` | `sum(8, deep(result))` | `sum(8, deep(this), deep(args), deep(result))` | 0 × 2/test | Input traversal and delegated conversions must be covered on success and raise. |

Kernel collection schedules retain their completion formulas. `list.clear`,
`map.clear`, `set.clear` and `record.clear` declare two units per shrinking
traversal test. `list.sort` declares two units per merge/copy test because a
comparator may shrink the original while the merge walks a captured copy.
`list.sort_by_keys` takes keys a caller computed once per item, so it runs
no callback: each merge pass makes at most one comparison per item it
places, and a comparison reads no further than the shallower key, so
`2 × (size(keys) + deep(keys))` per pass bounds its comparisons (FIG-6886).
Python's `py.sorted` applies the key once per item, as ordinary charged
callback code, then applies one ordinary closure that calls
`list.sort_by_keys`. The closure makes the native bill its own completion
formula, `sum(1, size(items), product(2, sum(size(keys), deep(keys)),
sum(1, ceil_log2(size(keys)))), size(result))`, even inside `py.method.sort`.
This boundary is necessary under K-CHG-007: nested library calls add no
completion charge, and the outer helper's arguments and result cannot
bound arbitrarily large captured keys returned by a callback. The wrappers
cover only linear work: `py.sorted` allows two input traversals and an
output traversal; `py.method.sort` allows four receiver traversals for
copying, key collection, clearing and replacement. It retains the metered
replacement loop. Neither wrapper reserves the old closure sort's
`256 × n × (1 + ceil_log2(n))` allowance (FIG-6904).
`tasks.wait_all` and `tasks.cancel_all` declare two units per task-set test;
their trip counts come from live tasks rather than call arguments.

## URI traversal bound (FIG-6720)

`ts.uri.decode` walks percent-delimited chunks. `text.split` makes two
linear passes. A chunk consumes one percent and two ASCII hex digits;
`int.parse("0" + pair, 16)` validates those digits together, rejecting signs,
whitespace, separators and non-ASCII digits. The remaining literal tail is
copied once. Octet runs are disjoint: `bytes.from_octets` and strict
`bytes.utf8_decode` each traverse a run once before it is discarded. Reserved
pairs flush the preceding run before their original spelling is appended.
No loop repeatedly indexes the full input or dispatches a hex helper per digit.

The completion bound reserves 32 fixed units for padding, conversion and
assembly, plus 64 per input unit for delegated parsing, tests, appends and
flushes, and the result size for final assembly. An escaped chunk consumes
at least three input units; even the reserved-pair-plus-literal branch uses
fewer than 48 delegated calls. Each call and statement also holds and releases operands. The 64-unit
coefficient includes these accounting changes and reservations as well as
the split, copy and UTF-8 passes; three units per escape give 192 units for
its bounded calls and bookkeeping. `ts.uri.decodeURI` and `ts.uri.decodeURIComponent`
apply that coefficient to `deep(args)`, covering the arguments' delegated
conversion as well as decoding. Guest callbacks keep their ordinary charge.
`ts.uri.flush` now includes the octet list size, so failed UTF-8 validation
is covered when there is no result. Raises use result size zero, as before.

| Helper | Previous completion formula | Completion formula |
| --- | --- | --- |
| `ts.uri.decode` | `sum(8, size(input), size(preserve), deep(result))` | `sum(32, product(64, size(input)), size(preserve), deep(result))` |
| `ts.uri.decodeURI`, `ts.uri.decodeURIComponent` | `sum(8, deep(args), deep(result))` | `sum(32, product(64, deep(args)), deep(result))` |
| `ts.uri.flush` | `sum(8, deep(result))` | `sum(8, size(octets), deep(result))` |

`helper_plans::uri_decode_completion_price_bounds_delegated_work_on_return_and_raise`
pins K-URI-WORK: delegated calls plus heap-accounting changes and
reservations stay below the operation's completion price
on successful and raised outcomes over growing escaped inputs, literal tails,
reserved escapes, malformed percent sequences and invalid UTF-8. The fixed
successful probe separately measures real work; a higher completion price
alone cannot satisfy the performance proof.
