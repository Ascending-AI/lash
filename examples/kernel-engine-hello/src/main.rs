use lash_kernel_engine::Datum;
use lash_kernel_engine::{
    Answer, ContractId, Dialect, Engine, External, Limits, Progress, Signature, Value,
};
use lash_kernel_engine::{Name, Param, Type};

fn main() -> Result<(), String> {
    let limits = Limits::default();
    let engine = Engine::new(limits.clone()).map_err(|e| e.to_string())?;
    let externals = [External {
        name: "host.greet".into(),
        contract: ContractId::new("hello-v1"),
        signature: Signature {
            params: vec![Param {
                name: Name::new("name"),
                ty: Type::Text,
                optional: false,
            }],
            result: Type::Text,
        },
    }];
    for (name, dialect, source, snippet) in [
        (
            "TypeScript",
            Dialect::TypeScript,
            include_str!("../hello.ts"),
            false,
        ),
        (
            "Python",
            Dialect::Python,
            include_str!("../hello.py"),
            false,
        ),
        (
            "snippet",
            Dialect::TypeScript,
            include_str!("../snippet.ts"),
            true,
        ),
        (
            "Python snippet",
            Dialect::Python,
            include_str!("../snippet.py"),
            true,
        ),
    ] {
        let program = if snippet {
            engine.compile_snippet(dialect, source, &externals)
        } else {
            engine.compile(dialect, source, &externals)
        }
        .map_err(|e| format!("{e:?}"))?;
        let args = if snippet {
            Vec::new()
        } else {
            vec![
                Value::new(Datum::Text(name.into()), limits.value_bytes)
                    .map_err(|e| e.to_string())?,
            ]
        };
        let turn = engine.run(&program, args).map_err(|e| e.to_string())?;
        let Progress::Paused {
            calls, snapshot, ..
        } = turn.state
        else {
            return Err("expected external call".into());
        };
        let [call] = calls.as_slice() else {
            return Err("expected one call".into());
        };
        let [arg] = call.args.as_slice() else {
            return Err("expected one argument".into());
        };
        let Datum::Text(who) = arg.data() else {
            return Err("expected text".into());
        };
        println!("{name} paused: {}({who})", call.name);
        let answer = Value::new(Datum::Text(format!("Hello, {who}!")), limits.value_bytes)
            .map_err(|e| e.to_string())?;
        let expected = answer.clone();
        let turn = engine
            .resume(
                snapshot,
                vec![Answer {
                    id: call.id.clone(),
                    result: Ok(answer),
                }],
            )
            .map_err(|e| e.to_string())?;
        match turn.state {
            Progress::Complete(Ok(value)) if value == expected => {
                println!("{name} result: {:?}", value.data())
            }
            _ => return Err("expected result".into()),
        }
    }
    Ok(())
}
