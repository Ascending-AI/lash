use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/url_search_params.kernel"),
        rows: vec![
            Row::Constructor {
                class: "URLSearchParams",
                length: Some(0),
                function: "ts.url_search_params.construct",
            },
            Row::InstanceOf {
                class: "URLSearchParams",
                function: "ts.url_search_params.is",
            },
            Row::Property {
                receiver: Receiver::Brand("url.whatwg.url_search_params"),
                name: "size",
                function: "ts.url_search_params.size",
            },
            Row::Method {
                path: "URLSearchParams.prototype.toString",
                length: 0,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.toString",
            },
            Row::Method {
                path: "URLSearchParams.prototype.valueOf",
                length: 0,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.valueOf",
            },
            Row::Method {
                path: "URLSearchParams.prototype.get",
                length: 1,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.get",
            },
            Row::Method {
                path: "URLSearchParams.prototype.getAll",
                length: 1,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.getAll",
            },
            Row::Method {
                path: "URLSearchParams.prototype.has",
                length: 1,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.has",
            },
            Row::Method {
                path: "URLSearchParams.prototype.append",
                length: 2,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.append",
            },
            Row::Method {
                path: "URLSearchParams.prototype.set",
                length: 2,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.set",
            },
            Row::Method {
                path: "URLSearchParams.prototype.delete",
                length: 1,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.delete",
            },
            Row::Method {
                path: "URLSearchParams.prototype.sort",
                length: 0,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.sort",
            },
            Row::Method {
                path: "URLSearchParams.prototype.keys",
                length: 0,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.keys",
            },
            Row::Method {
                path: "URLSearchParams.prototype.values",
                length: 0,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.values",
            },
            Row::Method {
                path: "URLSearchParams.prototype.entries",
                length: 0,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.entries",
            },
            Row::Method {
                path: "URLSearchParams.prototype.forEach",
                length: 1,
                receivers: &[Receiver::Brand("url.whatwg.url_search_params")],
                function: "ts.url_search_params.forEach",
            },
        ],
    }
}
