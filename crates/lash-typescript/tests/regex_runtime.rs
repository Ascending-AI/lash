use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, RuntimeError,
    State, Value, Vm, VmRunOutcome,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected RegExp test ability")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::compile(source)
        .unwrap_or_else(|error| panic!("compile `{source}`: {error}"));
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished(source: &str) -> Value {
    match execute(source).unwrap_or_else(|error| panic!("execute `{source}`: {error}")) {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn literals_constructor_exec_test_and_properties_are_exact() {
    assert_eq!(
        finished(
            "const r=/a(?<tail>b+)/gi; const m=r.exec('xxABBy'); finish([m[0],m[1],m.index,m.input,m.groups.tail,r.lastIndex,r.source,r.flags,r.global,r.ignoreCase]);"
        ),
        Value::List(
            vec![
                Value::String("ABB".into()),
                Value::String("BB".into()),
                Value::Number(2.0),
                Value::String("xxABBy".into()),
                Value::String("BB".into()),
                Value::Number(5.0),
                Value::String("a(?<tail>b+)".into()),
                Value::String("gi".into()),
                Value::Bool(true),
                Value::Bool(true),
            ]
            .into(),
        )
    );
    assert_eq!(
        finished(
            "const r=new RegExp('a+','y'); r.lastIndex=1; finish([r.test('baa'),r.lastIndex]);"
        ),
        Value::List(vec![Value::Bool(true), Value::Number(3.0)].into())
    );
    assert_eq!(
        finished("const s='x'.repeat(200000); finish(/z/y.test(s));"),
        Value::Bool(false)
    );
    assert_eq!(
        finished(
            "finish([new RegExp('/').source,new RegExp('[/]').source,new RegExp(undefined,'g').source]);"
        ),
        Value::List(
            vec![
                Value::String("\\/".into()),
                Value::String("[/]".into()),
                Value::String("(?:)".into()),
            ]
            .into()
        )
    );

    // A one-shot operation must stop after its answer. Scanning every later
    // empty match would consume the regex budget and make exec depend on an
    // irrelevant suffix.
    assert_eq!(
        finished("const s='x'.repeat(200000); finish(/(?:)/.exec(s).index);"),
        Value::Number(0.0)
    );
}

#[test]
fn string_regex_methods_replacements_and_match_all_are_exact() {
    assert_eq!(
        finished(
            "const r=/(?<a>a)(b)?/g; const ms=[...'aba'.matchAll(r)]; finish(['aba'.match(/a/g), 'aba'.search(/b/), 'aba'.replace(r,'<$<a>:$2>'), 'aba'.replaceAll('a','$$&'), 'a1b2'.split(/(\\d)/), ms[1].index, ms[0].groups.a]);"
        ),
        Value::List(
            vec![
                Value::List(vec![Value::String("a".into()), Value::String("a".into())].into()),
                Value::Number(1.0),
                Value::String("<a:b><a:>".into()),
                Value::String("$&b$&".into()),
                Value::List(
                    vec![
                        Value::String("a".into()),
                        Value::String("1".into()),
                        Value::String("b".into()),
                        Value::String("2".into()),
                        Value::String("".into()),
                    ]
                    .into(),
                ),
                Value::Number(2.0),
                Value::String("a".into()),
            ]
            .into(),
        )
    );
    assert_eq!(
        finished(
            "finish([...'aba'.matchAll(/(?<a>a)/g)].map(m=>`${m[0]}:${m.index}:${m.groups.a}`).join('|'));"
        ),
        Value::String("a:0:a|a:2:a".into())
    );
    assert_eq!(
        finished(
            "const m=/(?<x>a)(b)?/.exec('a'); const r=/a/y; r.lastIndex=1; const replaced='ba'.replace(r,'X'); finish([Object.keys(m),m.toString(),[...m],replaced,r.lastIndex]);"
        ),
        Value::List(
            vec![
                Value::List(
                    ["0", "1", "2", "index", "input", "groups"]
                        .into_iter()
                        .map(|value| Value::String(value.into()))
                        .collect::<Vec<_>>()
                        .into(),
                ),
                Value::String("a,a,".into()),
                Value::List(
                    vec![
                        Value::String("a".into()),
                        Value::String("a".into()),
                        Value::Undefined,
                    ]
                    .into(),
                ),
                Value::String("bX".into()),
                Value::Number(2.0),
            ]
            .into(),
        )
    );
    assert_eq!(
        finished(
            "finish(['ab'.split(/(?:)/),''.split(/(?:)/),'ba'.split(/a/y),'a1b2'.split(/(\\d)/,undefined)]);"
        ),
        Value::List(
            vec![
                Value::List(vec![Value::String("a".into()), Value::String("b".into())].into()),
                Value::List(Vec::new().into()),
                Value::List(vec![Value::String("b".into()), Value::String("".into())].into()),
                Value::List(
                    vec![
                        Value::String("a".into()),
                        Value::String("1".into()),
                        Value::String("b".into()),
                        Value::String("2".into()),
                        Value::String("".into()),
                    ]
                    .into()
                ),
            ]
            .into()
        )
    );
    assert_eq!(
        finished("const s='a'+'x'.repeat(200000); finish(s.split(/a/,1)[0]);"),
        Value::String("".into())
    );
    assert_eq!(
        finished("const s='a'+'x'.repeat(200000); finish(s.split(/(a)/,2).join('|'));"),
        Value::String("|a".into())
    );
    assert_eq!(
        finished(
            "const fake={}; fake['\\0lash.regexp.match']=true; fake.length=1e100; finish([Array.isArray(fake),Object.keys(fake).includes('\\0lash.regexp.match')]);"
        ),
        Value::List(vec![Value::Bool(false), Value::Bool(true)].into())
    );
    assert_eq!(
        finished(
            "const m=/(a)/.exec('a'); const mapped=m.map(x=>String(x)); m[0]='x'; m.index=7; m['input']='changed'; m.groups={nested:{value:'ok'}}; m.groups.nested.value='written'; m.length=1; finish([mapped,m[0],m.index,m.input,m.groups.nested.value,m.length]);"
        ),
        Value::List(
            vec![
                Value::List(vec![Value::String("a".into()), Value::String("a".into())].into()),
                Value::String("x".into()),
                Value::Number(7.0),
                Value::String("changed".into()),
                Value::String("written".into()),
                Value::Number(1.0),
            ]
            .into()
        )
    );
}

#[test]
fn function_replacers_receive_captures_offset_input_and_groups() {
    assert_eq!(
        finished(
            "const out='ab ab'.replaceAll(/(?<x>a)(b)/g,(m,a,b,i,s,g)=>`${m}:${a}:${b}:${i}:${s.length}:${g.x}`); finish(out);"
        ),
        Value::String("ab:a:b:0:5:a ab:a:b:3:5:a".into())
    );
    assert_eq!(
        finished(
            "const shared=[0]; const out='xx'.replace(/x/g,()=>{shared[0]=shared[0]+1;return shared;}); finish(out);"
        ),
        Value::String("12".into())
    );
    assert!(matches!(
        execute("'a'.replace(/a/, (match) => { print(match); return match; }); finish('wrong');"),
        Err(RuntimeError::EffectInBuiltinCallback)
    ));
}

#[test]
fn global_last_index_survives_a_real_park_between_exec_calls() {
    futures::executor::block_on(async {
        let program = lash_typescript::compile(
            "const r=/a/g; const first=r.exec('a a'); print(first.index); const second=r.exec('a a'); finish([first.index,second.index,r.lastIndex]);",
        )
        .expect("compile durable RegExp program");
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM");
        assert_eq!(
            vm.run_process_until_effect().await.expect("run to park"),
            VmRunOutcome::EffectCompleted
        );
        let continuation = vm.suspend().expect("suspend between exec calls");
        let wire = serde_json::to_vec(&continuation).expect("encode continuation");
        let restored = serde_json::from_slice(&wire).expect("restore continuation");
        let mut resumed = Vm::resume_from(restored, &program, &Host).expect("resume VM");
        let outcome = loop {
            match resumed
                .run_process_until_effect()
                .await
                .expect("complete resumed RegExp program")
            {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::Complete(outcome) => break outcome,
            }
        };
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(Value::List(
                vec![Value::Number(0.0), Value::Number(2.0), Value::Number(3.0)].into()
            ))
        );
    });
}

#[test]
fn invalid_dynamic_patterns_throw_syntax_error_objects() {
    assert_eq!(
        finished(
            "try { new RegExp('('); finish('wrong'); } catch (e) { finish([e.name,e instanceof SyntaxError]); }"
        ),
        Value::List(vec![Value::String("SyntaxError".into()), Value::Bool(true)].into())
    );
    assert_eq!(
        finished("try { new RegExp('('); finish('wrong'); } catch (e) { finish(e.message); }"),
        Value::String("Invalid regular expression: /(/: Unterminated group".into())
    );
    assert_eq!(
        finished("try { new RegExp('a','gg'); finish('wrong'); } catch (e) { finish(e.message); }"),
        Value::String("Invalid flags supplied to RegExp constructor 'gg'".into())
    );
    assert_eq!(
        finished(
            "try { new RegExp('a','d'); finish('wrong'); } catch (e) { finish([e.name,e.message]); }"
        ),
        Value::List(
            vec![
                Value::String("SyntaxError".into()),
                Value::String(
                    "TS_REGEX_INDICES_FLAG_UNSUPPORTED: Invalid flags supplied to RegExp constructor 'd'; remove `d` and use match.index plus capture lengths"
                        .into()
                ),
            ]
            .into()
        )
    );
    assert_eq!(
        finished(
            "function id(x){return x;} try { new RegExp(id(1)); finish('wrong'); } catch (e) { finish([e.name,e.message]); }"
        ),
        Value::List(
            vec![
                Value::String("TypeError".into()),
                Value::String(
                    "TS_REGEX_CONSTRUCTOR_STRING_REQUIRED: RegExp pattern and flags must be strings or undefined; pass an explicit string"
                        .into(),
                ),
            ]
            .into(),
        )
    );
}

#[test]
fn regexp_fuel_is_deterministic_and_uncatchable() {
    let input = "a".repeat(48);
    let source = format!(
        "try {{ /(a+)+b/.test('{input}'); finish('wrong'); }} catch (e) {{ finish('caught'); }} finally {{ finish('finally'); }}"
    );
    assert!(matches!(
        execute(&source),
        Err(RuntimeError::RegExpBudgetExceeded { limit })
            if limit == lashlang::TYPESCRIPT_REGEXP_EXECUTION_FUEL
    ));
}
