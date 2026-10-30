use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/abort.kernel"),
        rows: vec![
            Row::Constructor {
                class: "AbortController",
                length: Some(0),
                function: "ts.abort.construct",
            },
            Row::Class {
                class: "AbortSignal",
                length: 0,
            },
            Row::InstanceOf {
                class: "AbortController",
                function: "ts.abort.is_controller",
            },
            Row::InstanceOf {
                class: "AbortSignal",
                function: "ts.abort.is_signal",
            },
            Row::Property {
                receiver: Receiver::Brand("typescript.abort_controller"),
                name: "signal",
                function: "ts.abort.signal",
            },
            Row::Property {
                receiver: Receiver::Brand("typescript.abort_signal"),
                name: "aborted",
                function: "ts.abort.aborted",
            },
            Row::Property {
                receiver: Receiver::Brand("typescript.abort_signal"),
                name: "reason",
                function: "ts.abort.reason",
            },
            Row::Method {
                path: "AbortController.prototype.abort",
                length: 0,
                receivers: &[Receiver::Brand("typescript.abort_controller")],
                function: "ts.abort.abort",
            },
        ],
    }
}
