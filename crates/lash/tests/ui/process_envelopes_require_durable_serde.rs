#![allow(dead_code)]

use lash::process::{ObservedProcess, ObservedProcessEvent, ProcessTerminal};

fn store_process(process: &ObservedProcess) {
    let _ = serde_json::to_value(process);
}

fn store_terminal(terminal: &ProcessTerminal) {
    let _ = serde_json::to_value(terminal);
}

fn store_event(event: &ObservedProcessEvent) {
    let _ = serde_json::to_value(event);
}

fn main() {}
