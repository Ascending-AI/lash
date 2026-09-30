//! Capacity declared before overlapping PostgreSQL worker generations.

use serde::Serialize;

/// All persistent pools in one process must fit within `pool_max` in total.
/// `workers` counts other clients, including clients of other databases on
/// the same server. Administrative headroom includes PostgreSQL reserved slots.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PostgresConnectionBudget {
    pub processes_per_generation: u32,
    pub pool_max: u32,
    pub generations: u32,
    pub workers: u32,
    pub admin_headroom: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PostgresConnectionCapacity {
    pub max_connections: u32,
    pub reserved_connections: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PostgresConnectionBudgetReport {
    pub declared: PostgresConnectionBudget,
    pub server: PostgresConnectionCapacity,
    pub peak_connections: u64,
}

#[derive(Debug, Serialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum PostgresConnectionBudgetRefusal {
    InvalidConnectionBudget,
    ConnectionBudgetOverflow,
    ConnectionBudgetExceeded {
        report: PostgresConnectionBudgetReport,
    },
    ReservedConnectionsUnbudgeted {
        report: PostgresConnectionBudgetReport,
    },
}

impl std::fmt::Display for PostgresConnectionBudgetRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConnectionBudget => f.write_str("connection budget requires positive processes and pool max, and at least two overlapping generations"),
            Self::ConnectionBudgetOverflow => f.write_str("connection budget arithmetic overflow"),
            Self::ConnectionBudgetExceeded { report } => write!(f, "declared peak {} exceeds PostgreSQL max_connections {}", report.peak_connections, report.server.max_connections),
            Self::ReservedConnectionsUnbudgeted { report } => write!(f, "administrative headroom {} does not cover {} PostgreSQL reserved slots", report.declared.admin_headroom, report.server.reserved_connections),
        }
    }
}
impl std::error::Error for PostgresConnectionBudgetRefusal {}

impl PostgresConnectionBudget {
    pub fn peak_connections(self) -> Result<u64, PostgresConnectionBudgetRefusal> {
        if self.processes_per_generation == 0 || self.pool_max == 0 || self.generations < 2 {
            return Err(PostgresConnectionBudgetRefusal::InvalidConnectionBudget);
        }
        u64::from(self.processes_per_generation)
            .checked_mul(u64::from(self.pool_max))
            .and_then(|value| value.checked_mul(u64::from(self.generations)))
            .and_then(|value| value.checked_add(u64::from(self.workers)))
            .and_then(|value| value.checked_add(u64::from(self.admin_headroom)))
            .ok_or(PostgresConnectionBudgetRefusal::ConnectionBudgetOverflow)
    }

    pub fn check(
        self,
        server: PostgresConnectionCapacity,
    ) -> Result<PostgresConnectionBudgetReport, PostgresConnectionBudgetRefusal> {
        let report = PostgresConnectionBudgetReport {
            declared: self,
            server,
            peak_connections: self.peak_connections()?,
        };
        if self.admin_headroom < server.reserved_connections {
            return Err(PostgresConnectionBudgetRefusal::ReservedConnectionsUnbudgeted { report });
        }
        if report.peak_connections > u64::from(server.max_connections) {
            return Err(PostgresConnectionBudgetRefusal::ConnectionBudgetExceeded { report });
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_budget_refuses_overlap_that_single_generation_can_fit() {
        let capacity = PostgresConnectionCapacity {
            max_connections: 100,
            reserved_connections: 3,
        };
        let budget = PostgresConnectionBudget {
            processes_per_generation: 2,
            pool_max: 24,
            generations: 2,
            workers: 12,
            admin_headroom: 10,
        };
        assert!(2 * 24 + 12 + 10 <= capacity.max_connections);
        assert!(
            matches!(budget.check(capacity), Err(PostgresConnectionBudgetRefusal::ConnectionBudgetExceeded { report }) if report.peak_connections == 118)
        );
        let bounded = PostgresConnectionBudget {
            pool_max: 18,
            ..budget
        };
        assert_eq!(
            bounded
                .check(capacity)
                .expect("two generations fit")
                .peak_connections,
            94
        );
        assert!(
            matches!(PostgresConnectionBudget { generations: 3, ..bounded }.check(capacity), Err(PostgresConnectionBudgetRefusal::ConnectionBudgetExceeded { report }) if report.peak_connections == 130)
        );
        assert!(
            PostgresConnectionBudget {
                generations: 3,
                ..bounded
            }
            .check(PostgresConnectionCapacity {
                max_connections: 130,
                ..capacity
            })
            .is_ok()
        );
    }

    #[test]
    fn rolling_budget_reserves_server_slots_and_rejects_invalid_or_overflowing_sizes() {
        let capacity = PostgresConnectionCapacity {
            max_connections: 100,
            reserved_connections: 5,
        };
        let budget = PostgresConnectionBudget {
            processes_per_generation: 1,
            pool_max: 2,
            generations: 2,
            workers: 0,
            admin_headroom: 4,
        };
        assert!(matches!(
            budget.check(capacity),
            Err(PostgresConnectionBudgetRefusal::ReservedConnectionsUnbudgeted { .. })
        ));
        for invalid in [
            PostgresConnectionBudget {
                processes_per_generation: 0,
                ..budget
            },
            PostgresConnectionBudget {
                pool_max: 0,
                ..budget
            },
            PostgresConnectionBudget {
                generations: 1,
                ..budget
            },
        ] {
            assert!(matches!(
                invalid.check(capacity),
                Err(PostgresConnectionBudgetRefusal::InvalidConnectionBudget)
            ));
        }
        assert!(matches!(
            PostgresConnectionBudget {
                processes_per_generation: u32::MAX,
                pool_max: u32::MAX,
                generations: u32::MAX,
                ..budget
            }
            .check(capacity),
            Err(PostgresConnectionBudgetRefusal::ConnectionBudgetOverflow)
        ));
    }
}
