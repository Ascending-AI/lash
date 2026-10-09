use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/url_search_params.kernel"),
        rows: vec![
            Row::Constructor {
                class: "URLSearchParams",
                function: "ts.url_search_params.construct",
            },
            Row::InstanceOf {
                class: "URLSearchParams",
                function: "ts.url_search_params.is",
            },
            Row::Property {
                receiver: Receiver::Brand("url.search_params"),
                name: "size",
                function: "ts.url_search_params.size",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "toString",
                function: "ts.url_search_params.toString",
            },
            Row::Function {
                path: "URLSearchParams.prototype.toString",
                function: "ts.url_search_params.toString",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "valueOf",
                function: "ts.url_search_params.valueOf",
            },
            Row::Function {
                path: "URLSearchParams.prototype.valueOf",
                function: "ts.url_search_params.valueOf",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "get",
                function: "ts.url_search_params.get",
            },
            Row::Function {
                path: "URLSearchParams.prototype.get",
                function: "ts.url_search_params.get",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "getAll",
                function: "ts.url_search_params.getAll",
            },
            Row::Function {
                path: "URLSearchParams.prototype.getAll",
                function: "ts.url_search_params.getAll",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "has",
                function: "ts.url_search_params.has",
            },
            Row::Function {
                path: "URLSearchParams.prototype.has",
                function: "ts.url_search_params.has",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "append",
                function: "ts.url_search_params.append",
            },
            Row::Function {
                path: "URLSearchParams.prototype.append",
                function: "ts.url_search_params.append",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "set",
                function: "ts.url_search_params.set",
            },
            Row::Function {
                path: "URLSearchParams.prototype.set",
                function: "ts.url_search_params.set",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "delete",
                function: "ts.url_search_params.delete",
            },
            Row::Function {
                path: "URLSearchParams.prototype.delete",
                function: "ts.url_search_params.delete",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "sort",
                function: "ts.url_search_params.sort",
            },
            Row::Function {
                path: "URLSearchParams.prototype.sort",
                function: "ts.url_search_params.sort",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "keys",
                function: "ts.url_search_params.keys",
            },
            Row::Function {
                path: "URLSearchParams.prototype.keys",
                function: "ts.url_search_params.keys",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "values",
                function: "ts.url_search_params.values",
            },
            Row::Function {
                path: "URLSearchParams.prototype.values",
                function: "ts.url_search_params.values",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "entries",
                function: "ts.url_search_params.entries",
            },
            Row::Function {
                path: "URLSearchParams.prototype.entries",
                function: "ts.url_search_params.entries",
            },
            Row::Method {
                receiver: Receiver::Brand("url.search_params"),
                name: "forEach",
                function: "ts.url_search_params.forEach",
            },
            Row::Function {
                path: "URLSearchParams.prototype.forEach",
                function: "ts.url_search_params.forEach",
            },
        ],
    }
}
