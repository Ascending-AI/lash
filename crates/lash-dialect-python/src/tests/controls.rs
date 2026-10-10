//! Control calls: a call of a tool that ends the turn (FIG-5781), at parity
//! with the TypeScript dialect.
//!
//! A program ends its turn only by awaiting a control call at its top
//! level: `main` ends as soon as that call settles, so nothing after it
//! runs. A control call made into a task, or inside a function, could
//! settle beside work still running, and is refused before anything runs.

use lash_kernel_doc::{Datum, EffectName, Name, Param, Signature, Type};
use lash_kernel_vm::{Bindings, End, Outcome, Request};

use super::machine;

/// The laws' tools with `control_finish`, which ends the turn.
fn effects() -> std::collections::BTreeMap<EffectName, Signature> {
    let mut effects = machine::effects();
    effects.insert(
        EffectName::new("control_finish").expect("a tool's name"),
        Signature {
            params: vec![Param {
                name: Name::new("value"),
                ty: Type::Any,
                optional: false,
            }],
            result: Type::Any,
        },
    );
    effects
}

/// Law 1: the program ends where its control call settles. The print after
/// it never runs, and the call is the last thing it asked of its host.
#[test]
fn a_control_call_ends_the_program_and_nothing_after_it_runs() {
    let mut asked = Vec::new();
    let (lines, end) = machine::drive(
        "print('before')\nawait control_finish(1)\nprint('after')\n",
        effects(),
        Bindings::default(),
        |lines, park, _| {
            assert_eq!(lines, ["before"]);
            park.requests
                .iter()
                .map(|request| match request {
                    Request::Effect(effect) => {
                        asked.push(effect.effect.to_string());
                        (effect.wait, Outcome::Completed(Datum::Null))
                    }
                    Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
                })
                .collect()
        },
    );
    assert!(lines.is_empty(), "{lines:?}");
    assert!(matches!(end, End::Finished(_)), "{end:?}");
    assert_eq!(asked, ["control_finish"]);
}

/// Law 2: a control call made into a task (`asyncio.gather`,
/// `asyncio.create_task`) or inside a function is refused at lowering, so
/// no tool call of the program runs.
#[test]
fn a_control_call_not_awaited_at_the_top_level_is_refused_before_anything_runs() {
    for source in [
        "import asyncio\nawait asyncio.gather(control_finish(1), echo(2))\n",
        "import asyncio\njob = asyncio.create_task(control_finish(1))\nawait echo(2)\nawait job\n",
        "async def end():\n    await control_finish(1)\nawait end()\n",
    ] {
        let error = machine::lower_with_effects(source, &effects()).expect_err(source);
        assert_eq!(error.code, "PY_CONTROL_CALL_PLACEMENT", "{source}");
        assert_eq!(
            error.kind,
            lash_kernel_dialect::DiagnosticKind::ProgramDefect,
            "{source}"
        );
    }
    machine::lower_with_effects("await echo(1)\nawait control_finish(2)\n", &effects())
        .expect("a control call awaited at the top level lowers");
}

/// FIG-5781: a tool's name is reserved. A module binding named
/// `control_finish` would hide the tool from itself and every later cell,
/// so it is refused with a repair.
#[test]
fn a_cell_cannot_bind_a_tools_name() {
    for source in [
        "control_finish = 1\nprint(control_finish)\n",
        "def control_finish(x):\n    return x\nprint(control_finish(1))\n",
    ] {
        let error = machine::lower_with_effects(source, &effects()).expect_err(source);
        assert_eq!(error.code, "PY_SHADOWS_BUILTIN", "{source}");
        assert!(
            error
                .repairs
                .iter()
                .any(|repair| repair.contains("control_finish_")),
            "{source}"
        );
    }
}
