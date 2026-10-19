use super::{Object, Receiver, Row};

/// The receivers of every Array method: a list, and a regex match result,
/// which answers them as the list of its items.
const LISTS: &[Receiver] = &[Receiver::List, Receiver::Brand("regex.match")];

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
        path: "Array.prototype.at",
        receivers: LISTS,
        function: "ts.array.at",
    });
    rows.push(Row::Method {
        path: "Array.prototype.concat",
        receivers: LISTS,
        function: "ts.array.concat",
    });
    rows.push(Row::Method {
        path: "Array.prototype.copyWithin",
        receivers: LISTS,
        function: "ts.array.copyWithin",
    });
    rows.push(Row::Method {
        path: "Array.prototype.entries",
        receivers: LISTS,
        function: "ts.array.entries",
    });
    rows.push(Row::Method {
        path: "Array.prototype.every",
        receivers: LISTS,
        function: "ts.array.every",
    });
    rows.push(Row::Method {
        path: "Array.prototype.fill",
        receivers: LISTS,
        function: "ts.array.fill",
    });
    rows.push(Row::Method {
        path: "Array.prototype.filter",
        receivers: LISTS,
        function: "ts.array.filter",
    });
    rows.push(Row::Method {
        path: "Array.prototype.find",
        receivers: LISTS,
        function: "ts.array.find",
    });
    rows.push(Row::Method {
        path: "Array.prototype.findIndex",
        receivers: LISTS,
        function: "ts.array.findIndex",
    });
    rows.push(Row::Method {
        path: "Array.prototype.findLast",
        receivers: LISTS,
        function: "ts.array.findLast",
    });
    rows.push(Row::Method {
        path: "Array.prototype.findLastIndex",
        receivers: LISTS,
        function: "ts.array.findLastIndex",
    });
    rows.push(Row::Method {
        path: "Array.prototype.flat",
        receivers: LISTS,
        function: "ts.array.flat",
    });
    rows.push(Row::Method {
        path: "Array.prototype.flatMap",
        receivers: LISTS,
        function: "ts.array.flatMap",
    });
    rows.push(Row::Method {
        path: "Array.prototype.forEach",
        receivers: LISTS,
        function: "ts.array.forEach",
    });
    rows.push(Row::Method {
        path: "Array.prototype.includes",
        receivers: LISTS,
        function: "ts.array.includes",
    });
    rows.push(Row::Method {
        path: "Array.prototype.indexOf",
        receivers: LISTS,
        function: "ts.array.indexOf",
    });
    rows.push(Row::Method {
        path: "Array.prototype.join",
        receivers: LISTS,
        function: "ts.array.join",
    });
    rows.push(Row::Method {
        path: "Array.prototype.keys",
        receivers: LISTS,
        function: "ts.array.keys",
    });
    rows.push(Row::Method {
        path: "Array.prototype.lastIndexOf",
        receivers: LISTS,
        function: "ts.array.lastIndexOf",
    });
    rows.push(Row::Method {
        path: "Array.prototype.map",
        receivers: LISTS,
        function: "ts.array.map",
    });
    rows.push(Row::Method {
        path: "Array.prototype.pop",
        receivers: LISTS,
        function: "ts.array.pop",
    });
    rows.push(Row::Method {
        path: "Array.prototype.push",
        receivers: LISTS,
        function: "ts.array.push",
    });
    rows.push(Row::Method {
        path: "Array.prototype.reduce",
        receivers: LISTS,
        function: "ts.array.reduce",
    });
    rows.push(Row::Method {
        path: "Array.prototype.reduceRight",
        receivers: LISTS,
        function: "ts.array.reduceRight",
    });
    rows.push(Row::Method {
        path: "Array.prototype.reverse",
        receivers: LISTS,
        function: "ts.array.reverse",
    });
    rows.push(Row::Method {
        path: "Array.prototype.shift",
        receivers: LISTS,
        function: "ts.array.shift",
    });
    rows.push(Row::Method {
        path: "Array.prototype.slice",
        receivers: LISTS,
        function: "ts.array.slice",
    });
    rows.push(Row::Method {
        path: "Array.prototype.some",
        receivers: LISTS,
        function: "ts.array.some",
    });
    rows.push(Row::Method {
        path: "Array.prototype.sort",
        receivers: LISTS,
        function: "ts.array.sort",
    });
    rows.push(Row::Method {
        path: "Array.prototype.splice",
        receivers: LISTS,
        function: "ts.array.splice",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toLocaleString",
        receivers: LISTS,
        function: "ts.array.toLocaleString",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toReversed",
        receivers: LISTS,
        function: "ts.array.toReversed",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toSorted",
        receivers: LISTS,
        function: "ts.array.toSorted",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toSpliced",
        receivers: LISTS,
        function: "ts.array.toSpliced",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toString",
        receivers: LISTS,
        function: "ts.array.toString",
    });
    rows.push(Row::Method {
        path: "Array.prototype.unshift",
        receivers: LISTS,
        function: "ts.array.unshift",
    });
    rows.push(Row::Method {
        path: "Array.prototype.valueOf",
        receivers: LISTS,
        function: "ts.array.valueOf",
    });
    rows.push(Row::Method {
        path: "Array.prototype.values",
        receivers: LISTS,
        function: "ts.array.values",
    });
    rows.push(Row::Method {
        path: "Array.prototype.with",
        receivers: LISTS,
        function: "ts.array.with",
    });
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
