use super::{Object, Receiver, Row};

/// The receivers of every Array method: a list, and a regex match result,
/// which answers them as the list of its items.
const LISTS: &[Receiver] = &[
    Receiver::List,
    Receiver::Brand("typescript.regex_match"),
    Receiver::Brand("typescript.template"),
];

/// Array operations carry the JavaScript differences in kernel bodies.
pub(super) fn object() -> Object {
    let mut rows = vec![
        Row::Constructor {
            class: "Array",
            length: None,
            function: "ts.array.construct",
        },
        Row::Function {
            path: "Array",
            length: 1,
            function: "ts.array.construct",
        },
        Row::Function {
            path: "Array.from",
            length: 1,
            function: "ts.array.from",
        },
        Row::Function {
            path: "Array.of",
            length: 0,
            function: "ts.array.of",
        },
        Row::Function {
            path: "Array.isArray",
            length: 1,
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
        length: 1,
        receivers: LISTS,
        function: "ts.array.at",
    });
    rows.push(Row::Method {
        path: "Array.prototype.concat",
        length: 1,
        receivers: LISTS,
        function: "ts.array.concat",
    });
    rows.push(Row::Method {
        path: "Array.prototype.copyWithin",
        length: 2,
        receivers: LISTS,
        function: "ts.array.copyWithin",
    });
    rows.push(Row::Method {
        path: "Array.prototype.entries",
        length: 0,
        receivers: LISTS,
        function: "ts.array.entries",
    });
    rows.push(Row::Method {
        path: "Array.prototype.every",
        length: 1,
        receivers: LISTS,
        function: "ts.array.every",
    });
    rows.push(Row::Method {
        path: "Array.prototype.fill",
        length: 1,
        receivers: LISTS,
        function: "ts.array.fill",
    });
    rows.push(Row::Method {
        path: "Array.prototype.filter",
        length: 1,
        receivers: LISTS,
        function: "ts.array.filter",
    });
    rows.push(Row::Method {
        path: "Array.prototype.find",
        length: 1,
        receivers: LISTS,
        function: "ts.array.find",
    });
    rows.push(Row::Method {
        path: "Array.prototype.findIndex",
        length: 1,
        receivers: LISTS,
        function: "ts.array.findIndex",
    });
    rows.push(Row::Method {
        path: "Array.prototype.findLast",
        length: 1,
        receivers: LISTS,
        function: "ts.array.findLast",
    });
    rows.push(Row::Method {
        path: "Array.prototype.findLastIndex",
        length: 1,
        receivers: LISTS,
        function: "ts.array.findLastIndex",
    });
    rows.push(Row::Method {
        path: "Array.prototype.flat",
        length: 0,
        receivers: LISTS,
        function: "ts.array.flat",
    });
    rows.push(Row::Method {
        path: "Array.prototype.flatMap",
        length: 1,
        receivers: LISTS,
        function: "ts.array.flatMap",
    });
    rows.push(Row::Method {
        path: "Array.prototype.forEach",
        length: 1,
        receivers: LISTS,
        function: "ts.array.forEach",
    });
    rows.push(Row::Method {
        path: "Array.prototype.includes",
        length: 1,
        receivers: LISTS,
        function: "ts.array.includes",
    });
    rows.push(Row::Method {
        path: "Array.prototype.indexOf",
        length: 1,
        receivers: LISTS,
        function: "ts.array.indexOf",
    });
    rows.push(Row::Method {
        path: "Array.prototype.join",
        length: 1,
        receivers: LISTS,
        function: "ts.array.join",
    });
    rows.push(Row::Method {
        path: "Array.prototype.keys",
        length: 0,
        receivers: LISTS,
        function: "ts.array.keys",
    });
    rows.push(Row::Method {
        path: "Array.prototype.lastIndexOf",
        length: 1,
        receivers: LISTS,
        function: "ts.array.lastIndexOf",
    });
    rows.push(Row::Method {
        path: "Array.prototype.map",
        length: 1,
        receivers: LISTS,
        function: "ts.array.map",
    });
    rows.push(Row::Method {
        path: "Array.prototype.pop",
        length: 0,
        receivers: LISTS,
        function: "ts.array.pop",
    });
    rows.push(Row::Method {
        path: "Array.prototype.push",
        length: 1,
        receivers: LISTS,
        function: "ts.array.push",
    });
    rows.push(Row::Method {
        path: "Array.prototype.reduce",
        length: 1,
        receivers: LISTS,
        function: "ts.array.reduce",
    });
    rows.push(Row::Method {
        path: "Array.prototype.reduceRight",
        length: 1,
        receivers: LISTS,
        function: "ts.array.reduceRight",
    });
    rows.push(Row::Method {
        path: "Array.prototype.reverse",
        length: 0,
        receivers: LISTS,
        function: "ts.array.reverse",
    });
    rows.push(Row::Method {
        path: "Array.prototype.shift",
        length: 0,
        receivers: LISTS,
        function: "ts.array.shift",
    });
    rows.push(Row::Method {
        path: "Array.prototype.slice",
        length: 2,
        receivers: LISTS,
        function: "ts.array.slice",
    });
    rows.push(Row::Method {
        path: "Array.prototype.some",
        length: 1,
        receivers: LISTS,
        function: "ts.array.some",
    });
    rows.push(Row::Method {
        path: "Array.prototype.sort",
        length: 1,
        receivers: LISTS,
        function: "ts.array.sort_arguments",
    });
    rows.push(Row::Method {
        path: "Array.prototype.splice",
        length: 2,
        receivers: LISTS,
        function: "ts.array.splice",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toLocaleString",
        length: 0,
        receivers: LISTS,
        function: "ts.array.toLocaleString",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toReversed",
        length: 0,
        receivers: LISTS,
        function: "ts.array.toReversed",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toSorted",
        length: 1,
        receivers: LISTS,
        function: "ts.array.toSorted",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toSpliced",
        length: 2,
        receivers: LISTS,
        function: "ts.array.toSpliced",
    });
    rows.push(Row::Method {
        path: "Array.prototype.toString",
        length: 0,
        receivers: LISTS,
        function: "ts.array.toString",
    });
    rows.push(Row::Method {
        path: "Array.prototype.unshift",
        length: 1,
        receivers: LISTS,
        function: "ts.array.unshift",
    });
    rows.push(Row::Method {
        path: "Array.prototype.valueOf",
        length: 0,
        receivers: LISTS,
        function: "ts.array.valueOf",
    });
    rows.push(Row::Method {
        path: "Array.prototype.values",
        length: 0,
        receivers: LISTS,
        function: "ts.array.values",
    });
    rows.push(Row::Method {
        path: "Array.prototype.with",
        length: 2,
        receivers: LISTS,
        function: "ts.array.with",
    });
    rows.push(Row::Property {
        receiver: Receiver::Brand("typescript.regex_match"),
        name: "length",
        function: "ts.array.match_length",
    });
    rows.push(Row::Property {
        receiver: Receiver::Brand("typescript.template"),
        name: "length",
        function: "ts.array.match_length",
    });
    Object {
        source: include_str!("../helpers/array.kernel"),
        rows,
    }
}
