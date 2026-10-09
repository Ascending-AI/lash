use serde::Serialize;

pub const DEFAULT_STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

/// Stack configuration captured for this process; no occupancy is observed.
/// Capacity source identifies an explicit Tokio worker reservation, a Rust
/// thread default, or the process soft limit, in that order. These settings
/// do not describe every thread or child process executing a scenario.
#[derive(Debug, Clone, Serialize)]
pub struct StackProfile {
    pub worker_stack_bytes: Option<usize>,
    pub rust_min_stack_bytes: Option<usize>,
    pub process_stack_soft_limit_bytes: Option<u64>,
    pub process_stack_hard_limit_bytes: Option<u64>,
    pub process_stack_hard_limit_unlimited: bool,
    pub configured_stack_capacity_bytes: Option<u64>,
    pub configured_stack_capacity_source: Option<&'static str>,
    pub stack_budget_bytes: Option<usize>,
}

impl StackProfile {
    pub fn capture(worker_stack_bytes: Option<usize>, stack_budget_bytes: Option<usize>) -> Self {
        let rust_min_stack_bytes = std::env::var("RUST_MIN_STACK")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        let process_limit = linux_process_stack_limit();
        let process_stack_soft_limit_bytes = process_limit
            .as_ref()
            .and_then(|limit| limit.soft_limit_bytes);
        let process_stack_hard_limit_bytes = process_limit
            .as_ref()
            .and_then(|limit| limit.hard_limit_bytes);
        let process_stack_hard_limit_unlimited = process_limit
            .as_ref()
            .is_some_and(|limit| limit.hard_limit_unlimited);
        let (configured_stack_capacity_bytes, configured_stack_capacity_source) =
            if let Some(worker_stack_bytes) = worker_stack_bytes {
                (Some(worker_stack_bytes as u64), Some("tokio_worker"))
            } else if let Some(rust_min_stack_bytes) = rust_min_stack_bytes {
                (Some(rust_min_stack_bytes as u64), Some("rust_min_stack"))
            } else if let Some(process_stack_soft_limit_bytes) = process_stack_soft_limit_bytes {
                (
                    Some(process_stack_soft_limit_bytes),
                    Some("process_stack_soft_limit"),
                )
            } else {
                (None, None)
            };
        Self {
            worker_stack_bytes,
            rust_min_stack_bytes,
            process_stack_soft_limit_bytes,
            process_stack_hard_limit_bytes,
            process_stack_hard_limit_unlimited,
            configured_stack_capacity_bytes,
            configured_stack_capacity_source,
            stack_budget_bytes,
        }
    }
}

#[derive(Debug)]
struct ProcessStackLimit {
    soft_limit_bytes: Option<u64>,
    hard_limit_bytes: Option<u64>,
    hard_limit_unlimited: bool,
}

fn linux_process_stack_limit() -> Option<ProcessStackLimit> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max stack size"))?;
    let values = line["Max stack size".len()..]
        .split_whitespace()
        .collect::<Vec<_>>();
    if values.len() < 2 {
        return None;
    }
    Some(ProcessStackLimit {
        soft_limit_bytes: parse_limit_bytes(values[0]),
        hard_limit_bytes: parse_limit_bytes(values[1]),
        hard_limit_unlimited: values[1] == "unlimited",
    })
}

fn parse_limit_bytes(value: &str) -> Option<u64> {
    (value != "unlimited")
        .then(|| value.parse::<u64>().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_capacity_never_certifies_stack_occupancy() {
        let profile = serde_json::to_value(StackProfile::capture(Some(4096), Some(2048)))
            .expect("stack metadata serializes");
        println!("{profile}");
        assert_eq!(profile["configured_stack_capacity_bytes"], 4096);
        assert_eq!(profile["configured_stack_capacity_source"], "tokio_worker");
        assert!(profile.get("within_stack_budget").is_none());
        assert!(profile.get("measured_stack_bytes").is_none());
    }
}
