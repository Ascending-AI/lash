use anyhow::{Result, bail, ensure};
use lashlang::{AbilityOp, AbilityOutcome, ExecutionHostError, ExecutionOutcome, Value};

pub const AGENT: [&str; 10] = [
    "const orders = [{sku: 'A1', qty: 2, price: 3.5}, {sku: 'B2', qty: 1, price: 12.0}, {sku: 'C3', qty: 5, price: 1.0}];",
    "const lineTotal = (o: {qty: number, price: number}): number => o.qty * o.price; const firstLineTotal = lineTotal(orders[0]);",
    "const totals = [firstLineTotal, ...orders.slice(1).map(o => o.qty * o.price)];",
    "const grand = totals.reduce((sum, value) => sum + value, 0);",
    "const biggest = orders.reduce((best, o) => o.qty * o.price > best.qty * best.price ? o : best, orders[0]).sku;",
    "const summary = JSON.stringify({grand, biggest});",
    "const discount = grand > 10 ? 0.1 : 0;",
    "const net = Math.round(grand * (1 - discount) * 100) / 100;",
    "const report = `${orders.length} orders, total ${net}, biggest ${biggest}`;",
    "console.log(report);",
];
pub const GLOBALS: [&str; 11] = [
    "orders",
    "firstLineTotal",
    "totals",
    "grand",
    "biggest",
    "summary",
    "discount",
    "net",
    "report",
    "total",
    "values",
];

#[derive(Clone)]
pub struct Case {
    pub name: String,
    pub cells: Vec<String>,
    pub expected: Option<f64>,
    pub effects: usize,
    pub resumed: bool,
    pub error: bool,
}
impl Case {
    fn single(
        name: impl Into<String>,
        source: String,
        expected: Option<f64>,
        effects: usize,
    ) -> Self {
        Self {
            name: name.into(),
            cells: vec![source],
            expected,
            effects,
            resumed: false,
            error: false,
        }
    }
}
pub fn cases() -> Vec<Case> {
    let mut cases = vec![
        Case::single("zero-effects", "1 + 1;".into(), None, 0),
        Case::single("profiler-fresh", "finish(1 + 1);".into(), Some(2.0), 0),
        Case {
            name: "profiler-ten-feeds".into(),
            cells: AGENT.iter().map(|s| s.to_string()).collect(),
            expected: None,
            effects: 0,
            resumed: false,
            error: false,
        },
    ];
    for n in [0, 1, 10, 100] {
        cases.push(Case::single(format!("scalar-{n}"), format!("let total = 0; for (let i = 0; i < {n}; i++) {{ total += await tools.echo({{value: 1}}); }} finish(total);"), Some(n as f64), n));
        let calls = vec!["tools.echo({value: 1})"; n].join(",");
        let source = if n == 0 {
            "finish(0);".into()
        } else {
            format!(
                "const values = await Promise.all([{calls}]); finish(values.reduce((a, b) => a + b, 0));"
            )
        };
        cases.push(Case::single(
            format!("parallel-{n}"),
            source,
            Some(n as f64),
            n,
        ));
    }
    for size in [32, 8192, 1_044_480] {
        cases.push(Case::single(format!("value-{size}"), format!("const value = await tools.echo({{value: 'x'.repeat({size})}}); finish(value.length);"), Some(size as f64), 1));
    }
    let mut resumed = cases
        .iter()
        .find(|c| c.name == "scalar-10")
        .expect("scalar case exists")
        .clone();
    resumed.name = "resumed-segments".into();
    resumed.resumed = true;
    cases.push(resumed);
    let mut error = Case::single(
        "guest-error-replacement",
        "throw new Error('expected failure');".into(),
        None,
        0,
    );
    error.error = true;
    cases.push(error);
    cases
}
#[derive(Default)]
pub struct Host {
    pub effects: usize,
    pub printed: Vec<String>,
}
impl Host {
    pub fn perform(&mut self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(op) => {
                self.effects += 1;
                ensure_echo(&op.operation)?;
                Ok(AbilityOutcome::Value(
                    op.args
                        .first()
                        .and_then(Value::as_record)
                        .and_then(|r| r.get("value"))
                        .cloned()
                        .ok_or_else(|| ExecutionHostError::new("echo has no value"))?,
                ))
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                let mut results = Vec::new();
                for leaf in &batch.leaves {
                    let lashlang::ResourceOperationBatchLeaf::Operation(op) = leaf else {
                        return Err(ExecutionHostError::new("unexpected timer"));
                    };
                    let AbilityOutcome::Value(value) =
                        self.perform(AbilityOp::ResourceOperation(Box::new(op.clone())))?
                    else {
                        return Err(ExecutionHostError::new("non-value leaf"));
                    };
                    results.push(lashlang::ResourceOperationOutcome::Value(value));
                }
                Ok(AbilityOutcome::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::Print(Value::String(value)) => {
                self.printed.push(value.to_string());
                Ok(AbilityOutcome::Unit)
            }
            other => Err(ExecutionHostError::new(format!(
                "unexpected benchmark operation {other:?}"
            ))),
        }
    }
    pub fn check(&self, case: &Case, outcome: Option<&ExecutionOutcome>) -> Result<()> {
        ensure!(
            self.effects == case.effects,
            "{}: {} effects, expected {}",
            case.name,
            self.effects,
            case.effects
        );
        if let Some(expected) = case.expected {
            ensure!(
                outcome == Some(&ExecutionOutcome::Finished(Value::Number(expected))),
                "{}: wrong outcome {outcome:?}",
                case.name
            );
        } else if !case.error {
            ensure!(
                outcome == Some(&ExecutionOutcome::Continued),
                "{}: wrong outcome",
                case.name
            );
        }
        if case.cells.len() == 10 {
            ensure!(
                self.printed == ["3 orders, total 21.6, biggest B2"],
                "translated profiler report differs"
            );
        }
        if case.error && outcome.is_some() {
            bail!("guest error completed");
        }
        Ok(())
    }
}
fn ensure_echo(operation: &str) -> Result<(), ExecutionHostError> {
    if operation == "echo" {
        Ok(())
    } else {
        Err(ExecutionHostError::new("unadmitted operation"))
    }
}
pub fn environment() -> lashlang::LashlangHostEnvironment {
    lashlang::LashlangHostEnvironment::new(lashlang::LashlangHostCatalog::tool_default(["echo"]))
        .with_globals(GLOBALS)
}
