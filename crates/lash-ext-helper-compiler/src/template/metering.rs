//! Statement charges survive scalar partial evaluation, including folded calls.
use super::*;

pub(super) fn has_charges(steps: &[Step]) -> bool {
    steps.iter().any(|step| match step {
        Step::Meter(_) => true,
        Step::Alternatives { alternatives, .. } => {
            alternatives.iter().any(|(steps, _)| has_charges(steps))
        }
        _ => false,
    })
}

impl State {
    pub(super) fn charge(&mut self, form: &str) {
        if let Some(owner) = self.charging {
            let target = if self.heap { "heap" } else { "meter" };
            self.steps.push(Step::Meter(format!(
                "{target}.form(const {{ p::function({:?}) }}, p::FormCharge::{form});",
                owner.to_string()
            )));
        }
    }
}

impl Evaluator<'_> {
    pub(super) fn call(
        &mut self,
        function: FunctionId,
        args: Vec<Symbol>,
        state: &mut State,
        depth: u32,
    ) -> Option<Symbol> {
        let definition = self.library.definition(&function)?;
        let bill = state.charging.is_some() && definition.charge().is_some();
        let given = bill.then(|| {
            args.iter()
                .map(|arg| arg.code.clone())
                .chain(
                    (args.len()..definition.signature.params.len())
                        .map(|_| "Value::Absent".to_owned()),
                )
                .collect::<Vec<_>>()
                .join(", ")
        });
        let answer = self.call_inner(function, args, state, depth)?;
        if let Some(given) = given {
            let target = if self.heap { "heap" } else { "meter" };
            state.steps.push(Step::Meter(format!(
                "{target}.bill(const {{ p::function({:?}) }}, &[{given}], {});",
                function.to_string(),
                answer.code
            )));
        }
        Some(answer)
    }
}

pub(super) fn tree(code: &mut String, alternatives: &[(&[Step], &str)], pad: &str) {
    let mut at = 0;
    while let Some(&(steps, value)) = alternatives.get(at) {
        let first = steps.first().and_then(Step::pure_key);
        let run = match &first {
            Some(first) => alternatives[at..]
                .iter()
                .take_while(|(steps, _)| {
                    steps.first().and_then(Step::pure_key).as_ref() == Some(first)
                })
                .count(),
            None => 1,
        };
        let metered = metering::has_charges(steps);
        if metered {
            let _ = writeln!(code, "{pad}let charge_mark = meter.mark();");
        }
        let _ = writeln!(code, "{pad}if let Some(value) = (|| {{");
        match first {
            Some(_) if run > 1 => {
                let first = &steps[0];
                let _ = writeln!(code, "{pad}    {first}");
                let rest: Vec<_> = alternatives[at..at + run]
                    .iter()
                    .map(|&(steps, value)| (&steps[1..], value))
                    .collect();
                tree(code, &rest, &format!("{pad}    "));
                let _ = writeln!(code, "{pad}    None");
            }
            _ => {
                for step in steps {
                    let _ = writeln!(code, "{pad}    {step}");
                }
                let _ = writeln!(code, "{pad}    Some({value})");
            }
        }
        let _ = writeln!(code, "{pad}}})() {{ return Some(value); }}");
        if metered {
            let _ = writeln!(code, "{pad}meter.rewind(charge_mark);");
        }
        at += run;
    }
}
