use anyhow::{Result, ensure};
use rand_chacha::rand_core::RngCore;
use schemars::{JsonSchema, r#gen::SchemaGenerator, schema::Schema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Distribution(Vec<(u32, f64)>);

#[derive(JsonSchema)]
#[expect(dead_code, reason = "Schema-only numeric bounds for weighted pairs")]
struct Positive(#[schemars(range(min = 1))] u32);
#[derive(JsonSchema)]
#[expect(dead_code, reason = "Schema-only numeric bounds for weighted pairs")]
struct ByteSize(#[schemars(range(min = 128))] u32);
#[derive(JsonSchema)]
#[expect(dead_code, reason = "Schema-only numeric bounds for weighted pairs")]
struct Probability(#[schemars(range(min = 0, max = 1))] f64);

fn array_schema<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = <Vec<(T, Probability)>>::json_schema(generator);
    if let Schema::Object(object) = &mut schema {
        object.array().min_items = Some(1);
    }
    schema
}

impl JsonSchema for Distribution {
    fn schema_name() -> String {
        "Distribution".into()
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        array_schema::<Positive>(generator)
    }
}

pub(super) fn history_schema(generator: &mut SchemaGenerator) -> Schema {
    array_schema::<u32>(generator)
}
pub(super) fn bytes_schema(generator: &mut SchemaGenerator) -> Schema {
    array_schema::<ByteSize>(generator)
}

impl Distribution {
    pub fn buckets(&self) -> &[(u32, f64)] {
        &self.0
    }

    pub(super) fn validate(&self, name: &str, minimum: u32) -> Result<()> {
        ensure!(!self.0.is_empty(), "{name}: empty distribution");
        let mut seen = std::collections::BTreeSet::new();
        for &(value, probability) in &self.0 {
            ensure!(
                value >= minimum && seen.insert(value),
                "{name}: invalid or duplicate bucket {value}"
            );
            ensure!(
                probability.is_finite() && (0.0..=1.0).contains(&probability),
                "{name}: invalid probability"
            );
        }
        let total: f64 = self.0.iter().map(|(_, p)| p).sum();
        ensure!(
            (total - 1.0).abs() <= 1e-12,
            "{name}: probabilities must sum to one, got {total}"
        );
        Ok(())
    }

    pub(super) fn sample(&self, rng: &mut impl RngCore) -> u32 {
        let draw = unit(rng);
        let mut cumulative = 0.0;
        for &(value, probability) in &self.0 {
            cumulative += probability;
            if draw < cumulative {
                return value;
            }
        }
        self.0.last().map_or(0, |pair| pair.0)
    }
}

/// A uniform value strictly inside (0, 1), using exactly 52 random bits.
pub(super) fn unit(rng: &mut impl RngCore) -> f64 {
    ((rng.next_u64() >> 12) as f64 + 0.5) / 4_503_599_627_370_496.0
}
