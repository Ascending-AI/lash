//! Full-population tails, bounded independently of the first-record ledger.
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

use super::ledger::Observation;

const HIGH_US: u64 = 60_000_000;
pub(super) const MAX_POPULATIONS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Scope {
    pub backend: String,
    pub boundary: String,
    pub result: String,
    pub process_id: u32,
}
impl Scope {
    pub fn of(record: &Observation) -> Self {
        Self {
            backend: record.backend.clone(),
            boundary: record.boundary.clone(),
            result: record.result.clone(),
            process_id: record.process_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Slow {
    pub operation_id: String,
    pub duration_ns: u64,
    pub record_id: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Population {
    pub scope: Scope,
    pub count: u64,
    pub max_ns: u64,
    pub sub_microsecond: u64,
    pub zero: u64,
    pub overflow: u64,
    pub slowest: Vec<Slow>,
    #[serde(with = "buckets")]
    histogram: Histogram<u64>,
}

#[expect(clippy::expect_used, reason = "constant valid HDR bounds")]
fn histogram() -> Histogram<u64> {
    let mut histogram = Histogram::new_with_bounds(1, HIGH_US, 3).expect("fixed valid HDR bounds");
    histogram.auto(false);
    histogram
}
impl Population {
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            count: 0,
            max_ns: 0,
            sub_microsecond: 0,
            zero: 0,
            overflow: 0,
            slowest: Vec::new(),
            histogram: histogram(),
        }
    }
    #[expect(
        clippy::expect_used,
        reason = "monotonic operation intervals and checked fixed HDR bounds"
    )]
    pub fn record(&mut self, observation: &Observation) {
        let ns = u64::try_from(observation.end_ns - observation.start_ns)
            .expect("operation duration must fit nonnegative nanoseconds");
        self.count += 1;
        self.max_ns = self.max_ns.max(ns);
        if ns == 0 {
            self.zero += 1;
        }
        if ns < 1000 {
            self.sub_microsecond += 1;
        }
        if ns > HIGH_US * 1000 {
            self.overflow += 1;
        } else {
            self.histogram
                .record((ns / 1000).max(1))
                .expect("duration checked against fixed HDR bounds");
        }
        self.slowest.push(Slow {
            operation_id: observation.operation_id.clone(),
            duration_ns: ns,
            record_id: observation.record_id,
        });
        self.trim();
    }
    fn trim(&mut self) {
        self.slowest.sort_by(|a, b| {
            b.duration_ns
                .cmp(&a.duration_ns)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
        self.slowest.truncate(10);
    }
    #[expect(
        clippy::expect_used,
        reason = "all populations use the same fixed HDR bounds"
    )]
    pub fn merge(&mut self, other: Self) {
        self.count += other.count;
        self.max_ns = self.max_ns.max(other.max_ns);
        self.sub_microsecond += other.sub_microsecond;
        self.zero += other.zero;
        self.overflow += other.overflow;
        self.histogram
            .add(other.histogram)
            .expect("matching fixed histogram bounds");
        self.slowest.extend(other.slowest);
        self.trim();
    }
    fn quantile(&self, fraction: f64) -> serde_json::Value {
        let rank = (self.count as f64 * fraction).ceil() as u64;
        if rank > self.histogram.len() {
            return serde_json::json!(">60000000");
        }
        if rank == 0 {
            return serde_json::Value::Null;
        }
        // Rank is over the entire population, including censored observations.
        let mut cumulative = 0;
        for bucket in self.histogram.iter_recorded() {
            cumulative += bucket.count_at_value();
            if cumulative >= rank {
                return serde_json::json!(bucket.value_iterated_to().min(HIGH_US));
            }
        }
        unreachable!("in-range histogram rank")
    }
}
impl Serialize for Population {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::json!({
            "scope": self.scope, "count": self.count, "max_ns": self.max_ns,
            "sub_microsecond": self.sub_microsecond, "zero": self.zero, "overflow": self.overflow,
            "slowest": self.slowest, "histogram": {
                "low_us": 1, "high_us": HIGH_US, "significant_digits": 3,
                "auto_resize": false, "statistic": "full-population approximate nearest-rank",
                "p50_us": self.quantile(0.5), "p95_us": self.quantile(0.95),
                "p99_us": self.quantile(0.99), "p999_us": self.quantile(0.999),
                "p100_us": self.quantile(1.0),
                "buckets": self.histogram.iter_recorded().map(|v| (v.value_iterated_to().min(HIGH_US), v.count_at_value())).collect::<Vec<_>>()
            }
        }).serialize(serializer)
    }
}
mod buckets {
    use super::*;
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Histogram<u64>, D::Error> {
        #[derive(Deserialize)]
        struct Buckets {
            buckets: Vec<(u64, u64)>,
        }
        let value = Buckets::deserialize(deserializer)?;
        let mut result = histogram();
        for (value, count) in value.buckets {
            if value > HIGH_US {
                return Err(serde::de::Error::custom(
                    "HDR bucket exceeds declared range",
                ));
            }
            result
                .record_n(value, count)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(result)
    }
}
