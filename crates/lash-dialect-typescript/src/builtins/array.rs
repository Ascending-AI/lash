use super::{Object, Receiver, Row};

/// Array operations carry the JavaScript differences in kernel bodies.
pub(super) fn object() -> Object {
    let mut rows = vec![
        Row::Constructor {
            class: "Array",
            function: "ts.array.construct",
        },
        Row::Function {
            path: "Array",
            function: "ts.array.construct",
        },
        Row::Function {
            path: "Array.from",
            function: "ts.array.from",
        },
        Row::Function {
            path: "Array.of",
            function: "ts.array.of",
        },
        Row::Function {
            path: "Array.isArray",
            function: "ts.array.is_array",
        },
        Row::InstanceOf {
            class: "Array",
            function: "ts.array.is",
        },
        Row::Property {
            receiver: Receiver::List,
            name: "length",
            function: "ts.array.length_property",
        },
    ];
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "at",
        function: "ts.array.at",
    });
    rows.push(Row::Function {
        path: "Array.prototype.at",
        function: "ts.array.at",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "concat",
        function: "ts.array.concat",
    });
    rows.push(Row::Function {
        path: "Array.prototype.concat",
        function: "ts.array.concat",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "copyWithin",
        function: "ts.array.copyWithin",
    });
    rows.push(Row::Function {
        path: "Array.prototype.copyWithin",
        function: "ts.array.copyWithin",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "entries",
        function: "ts.array.entries",
    });
    rows.push(Row::Function {
        path: "Array.prototype.entries",
        function: "ts.array.entries",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "every",
        function: "ts.array.every",
    });
    rows.push(Row::Function {
        path: "Array.prototype.every",
        function: "ts.array.every",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "fill",
        function: "ts.array.fill",
    });
    rows.push(Row::Function {
        path: "Array.prototype.fill",
        function: "ts.array.fill",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "filter",
        function: "ts.array.filter",
    });
    rows.push(Row::Function {
        path: "Array.prototype.filter",
        function: "ts.array.filter",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "find",
        function: "ts.array.find",
    });
    rows.push(Row::Function {
        path: "Array.prototype.find",
        function: "ts.array.find",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "findIndex",
        function: "ts.array.findIndex",
    });
    rows.push(Row::Function {
        path: "Array.prototype.findIndex",
        function: "ts.array.findIndex",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "findLast",
        function: "ts.array.findLast",
    });
    rows.push(Row::Function {
        path: "Array.prototype.findLast",
        function: "ts.array.findLast",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "findLastIndex",
        function: "ts.array.findLastIndex",
    });
    rows.push(Row::Function {
        path: "Array.prototype.findLastIndex",
        function: "ts.array.findLastIndex",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "flat",
        function: "ts.array.flat",
    });
    rows.push(Row::Function {
        path: "Array.prototype.flat",
        function: "ts.array.flat",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "flatMap",
        function: "ts.array.flatMap",
    });
    rows.push(Row::Function {
        path: "Array.prototype.flatMap",
        function: "ts.array.flatMap",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "forEach",
        function: "ts.array.forEach",
    });
    rows.push(Row::Function {
        path: "Array.prototype.forEach",
        function: "ts.array.forEach",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "includes",
        function: "ts.array.includes",
    });
    rows.push(Row::Function {
        path: "Array.prototype.includes",
        function: "ts.array.includes",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "indexOf",
        function: "ts.array.indexOf",
    });
    rows.push(Row::Function {
        path: "Array.prototype.indexOf",
        function: "ts.array.indexOf",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "join",
        function: "ts.array.join",
    });
    rows.push(Row::Function {
        path: "Array.prototype.join",
        function: "ts.array.join",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "keys",
        function: "ts.array.keys",
    });
    rows.push(Row::Function {
        path: "Array.prototype.keys",
        function: "ts.array.keys",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "lastIndexOf",
        function: "ts.array.lastIndexOf",
    });
    rows.push(Row::Function {
        path: "Array.prototype.lastIndexOf",
        function: "ts.array.lastIndexOf",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "map",
        function: "ts.array.map",
    });
    rows.push(Row::Function {
        path: "Array.prototype.map",
        function: "ts.array.map",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "pop",
        function: "ts.array.pop",
    });
    rows.push(Row::Function {
        path: "Array.prototype.pop",
        function: "ts.array.pop",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "push",
        function: "ts.array.push",
    });
    rows.push(Row::Function {
        path: "Array.prototype.push",
        function: "ts.array.push",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "reduce",
        function: "ts.array.reduce",
    });
    rows.push(Row::Function {
        path: "Array.prototype.reduce",
        function: "ts.array.reduce",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "reduceRight",
        function: "ts.array.reduceRight",
    });
    rows.push(Row::Function {
        path: "Array.prototype.reduceRight",
        function: "ts.array.reduceRight",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "reverse",
        function: "ts.array.reverse",
    });
    rows.push(Row::Function {
        path: "Array.prototype.reverse",
        function: "ts.array.reverse",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "shift",
        function: "ts.array.shift",
    });
    rows.push(Row::Function {
        path: "Array.prototype.shift",
        function: "ts.array.shift",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "slice",
        function: "ts.array.slice",
    });
    rows.push(Row::Function {
        path: "Array.prototype.slice",
        function: "ts.array.slice",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "some",
        function: "ts.array.some",
    });
    rows.push(Row::Function {
        path: "Array.prototype.some",
        function: "ts.array.some",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "sort",
        function: "ts.array.sort",
    });
    rows.push(Row::Function {
        path: "Array.prototype.sort",
        function: "ts.array.sort",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "splice",
        function: "ts.array.splice",
    });
    rows.push(Row::Function {
        path: "Array.prototype.splice",
        function: "ts.array.splice",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "toLocaleString",
        function: "ts.array.toLocaleString",
    });
    rows.push(Row::Function {
        path: "Array.prototype.toLocaleString",
        function: "ts.array.toLocaleString",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "toReversed",
        function: "ts.array.toReversed",
    });
    rows.push(Row::Function {
        path: "Array.prototype.toReversed",
        function: "ts.array.toReversed",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "toSorted",
        function: "ts.array.toSorted",
    });
    rows.push(Row::Function {
        path: "Array.prototype.toSorted",
        function: "ts.array.toSorted",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "toSpliced",
        function: "ts.array.toSpliced",
    });
    rows.push(Row::Function {
        path: "Array.prototype.toSpliced",
        function: "ts.array.toSpliced",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "toString",
        function: "ts.array.toString",
    });
    rows.push(Row::Function {
        path: "Array.prototype.toString",
        function: "ts.array.toString",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "unshift",
        function: "ts.array.unshift",
    });
    rows.push(Row::Function {
        path: "Array.prototype.unshift",
        function: "ts.array.unshift",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "valueOf",
        function: "ts.array.valueOf",
    });
    rows.push(Row::Function {
        path: "Array.prototype.valueOf",
        function: "ts.array.valueOf",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "values",
        function: "ts.array.values",
    });
    rows.push(Row::Function {
        path: "Array.prototype.values",
        function: "ts.array.values",
    });
    rows.push(Row::Method {
        receiver: Receiver::List,
        name: "with",
        function: "ts.array.with",
    });
    rows.push(Row::Function {
        path: "Array.prototype.with",
        function: "ts.array.with",
    });
    let methods: Vec<_> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Method { name, function, .. } => Some((*name, *function)),
            _ => None,
        })
        .collect();
    rows.extend(methods.into_iter().map(|(name, function)| Row::Method {
        receiver: Receiver::Brand("regex.match"),
        name,
        function,
    }));
    rows.push(Row::Property {
        receiver: Receiver::Brand("regex.match"),
        name: "length",
        function: "ts.array.match_length",
    });
    Object {
        source: include_str!("../helpers/array.kernel"),
        rows,
    }
}
