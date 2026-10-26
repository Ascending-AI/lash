//! Bounded retained intervals. Aggregates carry their cardinality, never an averaged sample.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "measurement", rename_all = "snake_case")]
enum Measurement {
    Operation,
    Aggregate { operations: usize },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub record_id: usize,
    pub boundary: String,
    pub start_ns: i128,
    pub end_ns: i128,
    pub operation_id: String,
    pub result: String,
    pub backend: String,
    /// Offsets are relative to this process's Meter epoch, not comparable across processes.
    pub process_id: u32,
    #[serde(flatten)]
    measurement: Measurement,
}
impl Observation {
    pub fn operation(
        boundary: &str,
        id: String,
        result: &str,
        start_ns: i128,
        end_ns: i128,
    ) -> Self {
        Self {
            record_id: 0,
            boundary: boundary.into(),
            start_ns,
            end_ns,
            operation_id: id,
            result: result.into(),
            backend: String::new(),
            process_id: std::process::id(),
            measurement: Measurement::Operation,
        }
    }
    pub fn aggregate(
        boundary: &str,
        operations: usize,
        id: String,
        result: &str,
        start_ns: i128,
        end_ns: i128,
    ) -> Self {
        Self {
            measurement: Measurement::Aggregate { operations },
            ..Self::operation(boundary, id, result, start_ns, end_ns)
        }
    }
    pub fn is_operation(&self) -> bool {
        matches!(self.measurement, Measurement::Operation)
    }
    pub fn operations(&self) -> usize {
        match self.measurement {
            Measurement::Operation => 1,
            Measurement::Aggregate { operations } => operations,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Ledger {
    kind: String,
    pub cap: usize,
    pub dropped_operations: usize,
    pub dropped_records: usize,
    pub observations: Vec<Observation>,
    pub(crate) populations: Vec<super::tails::Population>,
    pub untracked_population_records: usize,
}
impl Ledger {
    pub fn new(cap: usize) -> Self {
        Self {
            kind: "lash.boundary-observations".into(),
            cap,
            dropped_operations: 0,
            dropped_records: 0,
            observations: Vec::new(),
            populations: Vec::new(),
            untracked_population_records: 0,
        }
    }
    pub fn push(&mut self, mut observation: Observation) {
        observation.record_id = self.observations.len() + self.dropped_records;
        if observation.is_operation() {
            let scope = super::tails::Scope::of(&observation);
            if let Some(population) = self.populations.iter_mut().find(|p| p.scope == scope) {
                population.record(&observation);
            } else if self.populations.len() < super::tails::MAX_POPULATIONS {
                let mut population = super::tails::Population::new(scope);
                population.record(&observation);
                self.populations.push(population);
            } else {
                self.untracked_population_records += 1;
            }
        }
        self.retain(observation);
    }
    fn retain(&mut self, observation: Observation) {
        if self.observations.len() < self.cap {
            self.observations.push(observation);
        } else {
            self.dropped_operations += observation.operations();
            self.dropped_records += 1;
        }
    }
    pub fn with_backend(&self, backend: &str) -> Self {
        let mut ledger = self.clone();
        for observation in &mut ledger.observations {
            if observation.backend.is_empty() {
                observation.backend = backend.into();
            }
        }
        for population in &mut ledger.populations {
            if population.scope.backend.is_empty() {
                population.scope.backend = backend.into();
            }
        }
        ledger
    }
    pub fn merge(&mut self, child: Self) {
        self.dropped_operations += child.dropped_operations;
        self.dropped_records += child.dropped_records;
        self.untracked_population_records += child.untracked_population_records;
        for population in child.populations {
            if let Some(existing) = self
                .populations
                .iter_mut()
                .find(|p| p.scope == population.scope)
            {
                existing.merge(population);
            } else if self.populations.len() < super::tails::MAX_POPULATIONS {
                self.populations.push(population);
            } else {
                self.untracked_population_records += population.count as usize;
            }
        }
        for observation in child.observations {
            self.retain(observation);
        }
    }
}
