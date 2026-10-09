//! These are kernel text, with callback calls at statement position.
//! A wait therefore needs no callback driver or private continuation.

// Signature, body, charge formula, declared errors. Dependencies are resolved
// by identity at registration, in this order (a body only uses earlier entries).
pub(super) fn functions() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    let mut bodies = vec![
        (
            "list.set(xs: List(Any), index: Number, value: Any) -> List(Any)",
            "do invoke list.check(xs) set xs[index] = value return xs",
            "sum(1, size(index))",
            "",
        ),
        (
            "list.push(xs: List(Any), value: Any) -> Int",
            "set xs[list.len(xs)] = value return list.len(xs)",
            "1",
            "",
        ),
        (
            "list.pop(xs: List(Any)) -> Any",
            "let n = list.len(xs) if eq(n, 0) { return absent } let i = num.sub(n, 1) let value = xs[i] remove xs[i] return value",
            "1",
            "",
        ),
        (
            "list.remove(xs: List(Any), index: Number) -> Any",
            "let value = list.get(xs, index) remove xs[index] return value",
            "sum(1, size(xs), size(index))",
            "",
        ),
        (
            "list.insert(xs: List(Any), index: Number, value: Any) -> List(Any)",
            "let i = list.insert_index(xs, index) let n = list.len(xs) if eq(i, n) { set xs[n] = value return xs } \
             set xs[n] = xs[num.sub(n, 1)] let j = num.sub(n, 1) while num.lt(i, j) { \
             set xs[j] = xs[num.sub(j, 1)] set j = num.sub(j, 1) } set xs[i] = value return xs",
            "sum(1, size(xs), size(index))",
            "",
        ),
        (
            "list.clear(xs: List(Any)) -> List(Any)",
            "do invoke list.check(xs) while num.lt(0, list.len(xs)) { remove xs[num.sub(list.len(xs), 1)] } return xs",
            "sum(1, size(xs))",
            "",
        ),
        (
            "map.set(xs: Map(Any, Any), key: Any, value: Any) -> Map(Any, Any)",
            "do invoke map.check(xs) set xs[key] = value return xs",
            "sum(1, size(xs), deep(key))",
            "",
        ),
        (
            "map.remove(xs: Map(Any, Any), key: Any) -> Bool",
            "let had = map.contains(xs, key) remove xs[key] return had",
            "sum(1, size(xs), deep(key))",
            "",
        ),
        (
            "map.clear(xs: Map(Any, Any)) -> Map(Any, Any)",
            "let keys = map.keys(xs) for key in keys { remove xs[key] } return xs",
            "sum(1, size(xs))",
            "",
        ),
        (
            "set.insert(xs: Set(Any), key: Any) -> Set(Any)",
            "do invoke set.check(xs) set xs[key] = true return xs",
            "sum(1, size(xs), deep(key))",
            "",
        ),
        (
            "set.remove(xs: Set(Any), key: Any) -> Bool",
            "let had = set.contains(xs, key) remove xs[key] return had",
            "sum(1, size(xs), deep(key))",
            "",
        ),
        (
            "set.clear(xs: Set(Any)) -> Set(Any)",
            "let keys = set.keys(xs) for key in keys { remove xs[key] } return xs",
            "sum(1, size(xs))",
            "",
        ),
        (
            "record.set(xs: Record{..Any}, key: Text, value: Any) -> Record{..Any}",
            "do invoke record.check(xs) set xs[key] = value return xs",
            "sum(1, size(xs), size(key))",
            "",
        ),
        (
            "record.remove(xs: Record{..Any}, key: Text) -> Bool",
            "let had = record.contains(xs, key) remove xs[key] return had",
            "sum(1, size(xs), size(key))",
            "",
        ),
        (
            "record.clear(xs: Record{..Any}) -> Record{..Any}",
            "let keys = record.keys(xs) for key in keys { remove xs[key] } return xs",
            "sum(1, size(xs))",
            "",
        ),
        (
            "collection.map(xs: Any, f: Fn(x: Any) -> Any) -> List(Any)",
            "do invoke collection.function_check(f) let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out",
            "sum(1, size(xs), size(result))",
            "",
        ),
        (
            "collection.filter(xs: Any, f: Fn(x: Any) -> Bool) -> List(Any)",
            "do invoke collection.function_check(f) let out = [] for x in xs { let keep = apply f(x) if keep { set out[list.len(out)] = x } } return out",
            "sum(1, size(xs), size(result))",
            "",
        ),
        (
            "collection.reduce(xs: Any, f: Fn(acc: Any, x: Any) -> Any, initial: Any) -> Any",
            "do invoke collection.function_check(f) let acc = initial for x in xs { let next = apply f(acc, x) set acc = next } return acc",
            "sum(1, size(xs))",
            "",
        ),
        (
            "collection.find(xs: Any, f: Fn(x: Any) -> Bool) -> Any",
            "do invoke collection.function_check(f) for x in xs { let found = apply f(x) if found { return x } } return absent",
            "sum(1, size(xs))",
            "",
        ),
        (
            "collection.any(xs: Any, f: Fn(x: Any) -> Bool) -> Bool",
            "do invoke collection.function_check(f) for x in xs { let found = apply f(x) if found { return true } } return false",
            "sum(1, size(xs))",
            "",
        ),
        (
            "collection.all(xs: Any, f: Fn(x: Any) -> Bool) -> Bool",
            "do invoke collection.function_check(f) for x in xs { let found = apply f(x) if found { } else { return false } } return true",
            "sum(1, size(xs))",
            "",
        ),
        (
            "collection.for_each(xs: Any, f: Fn(x: Any) -> Any) -> Null",
            "do invoke collection.function_check(f) for x in xs { do apply f(x) } return null",
            "sum(1, size(xs))",
            "",
        ),
        (
            "list.sort(xs: List(Any), compare: Fn(a: Any, b: Any) -> Number) -> List(Any)",
            "do invoke collection.function_check(compare) let sorted = list.copy(xs) let n = list.len(sorted) let i = 1 \
             while num.lt(i, n) { let value = sorted[i] let j = i \
             while num.lt(0, j) { let k = num.sub(j, 1) let before = sorted[k] \
             let order = apply compare(before, value) \
             if eq(order, order) { } else { do invoke collection.unordered(order) } \
             if num.lt(0, order) { set sorted[j] = before set j = k } else { break } } \
             set sorted[j] = value set i = num.add(i, 1) } \
             do invoke list.clear(xs) for value in sorted { set xs[list.len(xs)] = value } return xs",
            "sum(1, product(size(xs), size(xs)))",
            "errors \"unordered\"",
        ),
        (
            "collection.sum(xs: Any) -> Number",
            "let total = 0 for x in xs { set total = num.add(total, x) } return total",
            "sum(1, deep(xs))",
            "",
        ),
        (
            "collection.group_by(xs: Any, f: Fn(x: Any) -> Any) -> Map(Any, Any)",
            "do invoke collection.function_check(f) let out = map{} for x in xs { let key = apply f(x) \
             if map.contains(out, key) { let group = out[key] set group[list.len(group)] = x } \
             else { set out[key] = [x] } } return out",
            "sum(1, size(xs), deep(result))",
            "",
        ),
        (
            "collection.zip(xs: List(Any), ys: List(Any)) -> List(Tuple(Any, Any))",
            "let out = [] let i = 0 let n = list.len(xs) let m = list.len(ys) \
             while num.lt(i, n) { if num.lt(i, m) { set out[i] = (xs[i], ys[i]) } else { break } \
             set i = num.add(i, 1) } return out",
            "sum(1, size(xs), size(ys), size(result))",
            "",
        ),
        (
            "collection.enumerate(xs: Any) -> List(Tuple(Int, Any))",
            "let out = [] let i = 0 for x in xs { set out[i] = (i, x) set i = num.add(i, 1) } return out",
            "sum(1, size(xs), size(result))",
            "",
        ),
        (
            "collection.range(start: Int, end: Int, step?: Int) -> List(Int)",
            "do invoke collection.int_check(start) do invoke collection.int_check(end) \
             if eq(step, absent) { set step = 1 } do invoke collection.int_check(step) \
             if eq(step, 0) { do invoke collection.zero_step(step) } \
             let out = [] let i = start \
             if num.lt(0, step) { while num.lt(i, end) { set out[list.len(out)] = i set i = num.add(i, step) } } \
             else { while num.lt(end, i) { set out[list.len(out)] = i set i = num.add(i, step) } } return out",
            "sum(1, size(start), size(end), size(step), size(result))",
            "errors \"zero_step\"",
        ),
        (
            "tasks.wait_all() -> List(Any)",
            "let out = [] while true { let handles = tasks.unfinished() if eq(list.len(handles), 0) { break } \
             let values = join all handles for value in values { set out[list.len(out)] = value } } return out",
            "sum(1, size(result))",
            "",
        ),
        (
            "tasks.cancel_all() -> List(Any)",
            "let out = [] while true { let handles = tasks.unfinished() if eq(list.len(handles), 0) { break } \
             for handle in handles { do cancel handle } let outcomes = join settled handles \
             for outcome in outcomes { set out[list.len(out)] = outcome } } return out",
            "sum(1, size(result))",
            "",
        ),
    ];
    // Key insertion is the same operation as writing a map entry. It has its
    // own definition identity, without a second implementation or an alias.
    bodies.insert(
        7,
        (
            "map.insert(xs: Map(Any, Any), key: Any, value: Any) -> Map(Any, Any)",
            "do invoke map.check(xs) set xs[key] = value return xs",
            "sum(1, size(xs), deep(key))",
            "",
        ),
    );
    bodies
}
