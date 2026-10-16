//! Operator-owned datasets. Python's standard TOML parser keeps this example
//! independent of a second manifest parser dependency.
use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(super) enum Floor {
    Economy,
    Frontier,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Tier {
    Mechanism,
    Skill,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Scenario {
    Conversation,
    KillRestart,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Model {
    pub id: String,
    pub model: String,
    pub floor: Floor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rubric {
    pub id: String,
    pub weight: u32,
    pub criterion: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Case {
    pub id: String,
    pub tier: Tier,
    pub driver_floor: Floor,
    pub scenario: Scenario,
    pub inputs: Vec<String>,
    pub expectations: String,
    pub rubric: Vec<Rubric>,
    pub pass_score: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub name: String,
    pub repeats: usize,
    pub timeout_seconds: u64,
    pub score_min: u8,
    pub score_max: u8,
    pub arm_order: Vec<String>,
    pub judge: Model,
    pub models: Vec<Model>,
    pub cases: Vec<Case>,
}

fn path_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

impl Manifest {
    pub fn parse(toml: &str) -> Result<Self> {
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import json,sys,tomllib; json.dump(tomllib.loads(sys.stdin.read()),sys.stdout)",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start Python 3.11+ TOML parser")?;
        child
            .stdin
            .take()
            .context("TOML parser stdin")?
            .write_all(toml.as_bytes())?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "invalid TOML: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest: Self =
            serde_json::from_slice(&output.stdout).context("decode eval manifest")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "dataset name is empty");
        ensure!((1..=100).contains(&self.repeats), "repeats must be 1..100");
        ensure!(
            (1..=600).contains(&self.timeout_seconds),
            "timeout_seconds must be 1..600"
        );
        ensure!(
            self.score_min == 0 && self.score_max == 4,
            "rubric scale must be 0–4"
        );
        ensure!(
            self.judge.floor == Floor::Frontier,
            "judge must be frontier-tier"
        );
        ensure!(
            !self.judge.model.trim().is_empty(),
            "judge model identity is empty"
        );
        ensure!(
            !self.models.is_empty() && !self.cases.is_empty(),
            "dataset needs models and cases"
        );
        let mut model_ids = BTreeSet::new();
        for model in &self.models {
            ensure!(
                path_id(&model.id) && model_ids.insert(&model.id),
                "invalid or duplicate model id"
            );
            ensure!(!model.model.trim().is_empty(), "model identity is empty");
        }
        let order: BTreeSet<_> = self.arm_order.iter().collect();
        ensure!(
            order == model_ids && self.arm_order.len() == model_ids.len(),
            "arm_order must name every model exactly once"
        );
        let mut cases = BTreeSet::new();
        for case in &self.cases {
            ensure!(
                path_id(&case.id) && cases.insert(&case.id),
                "invalid or duplicate case id"
            );
            ensure!(
                case.tier != Tier::Skill || case.driver_floor == Floor::Frontier,
                "skill cases require a frontier driver"
            );
            ensure!(
                self.models.iter().all(|m| m.floor >= case.driver_floor),
                "model below case driver floor"
            );
            ensure!(
                !case.inputs.is_empty() && case.inputs.iter().all(|s| !s.trim().is_empty()),
                "case inputs are empty"
            );
            ensure!(
                case.scenario != Scenario::KillRestart || case.inputs.len() >= 2,
                "kill_restart needs prior history and a continuation input"
            );
            ensure!(
                !case.expectations.trim().is_empty(),
                "case expectations are empty"
            );
            ensure!(
                case.pass_score.is_finite() && (0.0..=4.0).contains(&case.pass_score),
                "pass_score outside 0–4"
            );
            ensure!(!case.rubric.is_empty(), "case rubric is empty");
            let mut rubric_ids = BTreeSet::new();
            let mut sum = 0_u32;
            for item in &case.rubric {
                ensure!(
                    path_id(&item.id) && rubric_ids.insert(&item.id),
                    "invalid or duplicate rubric id"
                );
                ensure!(
                    item.weight > 0 && !item.criterion.trim().is_empty(),
                    "rubric needs positive weights and criteria"
                );
                sum = sum
                    .checked_add(item.weight)
                    .context("rubric weights overflow")?;
            }
        }
        Ok(())
    }
}
