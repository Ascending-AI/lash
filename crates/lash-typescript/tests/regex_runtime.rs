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
    let program = lash_typescript::testing::compile(source)
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

/// GetSubstitution's numeric-reference rule, end to end: `$0` and `$00` are
/// index 0 and never name a capture, `$01` names capture 1, and an
/// out-of-range `$nn` falls back to `$n` plus a literal digit before staying
/// literal itself — under `replace` and `replaceAll` alike (FIG-3649).
#[test]
fn replacement_dollar_digits_follow_get_substitution() {
    let cases = [
        ("finish('foo-x-bar'.replace(/(x)/,'|$0|'));", "foo-|$0|-bar"),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$00|'));",
            "foo-|$00|-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$000|'));",
            "foo-|$000|-bar",
        ),
        ("finish('foo-x-bar'.replace(/(x)/,'|$01|'));", "foo-|x|-bar"),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$010|'));",
            "foo-|x0|-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/((((((((((x))))))))))/,'|$10|'));",
            "foo-|x|-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$10|'));",
            "foo-|x0|-bar",
        ),
        ("finish('foo-x-bar'.replace('x','|$1|'));", "foo-|$1|-bar"),
        ("finish('foo-x-bar'.replace('x','|$01|'));", "foo-|$01|-bar"),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$02|'));",
            "foo-|$02|-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$99|'));",
            "foo-|$99|-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)/,'|$11|'));",
            "foo-|x1|-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)|(y)/,'|$2|'));",
            "foo-||-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)|(y)/,'|$02|'));",
            "foo-||-bar",
        ),
        ("finish('x-x'.replaceAll(/(x)/g,'$01'));", "x-x"),
        (
            "finish('x-x'.replaceAll(/(x)/g,'|$0|$1|'));",
            "|$0|x|-|$0|x|",
        ),
        ("finish('x-x'.replaceAll('x','|$1|'));", "|$1|-|$1|"),
        (
            "finish('foo-x-bar'.replace(/(?<n>x)/,'|$<n>|$<miss>|'));",
            "foo-|x||-bar",
        ),
        (
            "finish('foo-x-bar'.replace(/(x)/,\"|$$|$&|$`|$'|\"));",
            "foo-|$|x|foo-|-bar|-bar",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
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
        let program = lash_typescript::testing::compile(
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

/// A host with an instruction budget bounds regexp work too.
///
/// The per-call fuel bounds one match; nothing bounded a program that made a
/// lot of them, so N instructions bought N million regexp steps and the only
/// bound a host had on total work said nothing about the engine. Each granted
/// allowance is now charged to the instruction budget, so a regexp-heavy loop
/// runs out of budget instead of running unbounded.
struct BudgetedHost {
    instructions: std::num::NonZeroU64,
}

impl ExecutionHost for BudgetedHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected budgeted ability")),
        }
    }

    fn execution_bounds(&self) -> lashlang::ExecutionBounds {
        lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::Bounded(self.instructions),
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Bounded(lashlang::DEFAULT_HOST_MEMORY_LIMIT_BYTES),
        )
    }
}

fn execute_budgeted(source: &str, instructions: u64) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("compile `{source}`: {error}"));
    let host = BudgetedHost {
        instructions: std::num::NonZeroU64::new(instructions).expect("nonzero budget"),
    };
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
}

#[test]
fn a_regexp_heavy_loop_exhausts_the_instruction_budget() {
    // Each iteration matches trivially and returns instantly, so nothing here
    // trips the per-call fuel: only the charge links this loop to the budget.
    let budget = 5_000;
    let loop_body = |work: &str| {
        format!(
            "let hits = 0;\nfor (let i = 0; i < 100; i++) {{\n  if ({work}) {{ hits++; }}\n}}\nfinish(hits);"
        )
    };

    // The control: the same loop, same budget, without the regexp. It has to
    // complete, or this test would be measuring the loop rather than the
    // regexp charge.
    assert_eq!(
        execute_budgeted(&loop_body("'xxabbbc'.includes('abbbc')"), budget)
            .expect("the same loop without a regexp must fit the budget"),
        ExecutionOutcome::Finished(Value::Number(100.0))
    );

    assert!(
        matches!(
            execute_budgeted(&loop_body("/ab+c/.test('xxabbbc')"), budget),
            Err(RuntimeError::InstructionBudgetExceeded { limit }) if limit == budget
        ),
        "a regexp loop must be bounded by the instruction budget"
    );

    // The charge is a ratio, not a ban: the same loop finishes when the budget
    // covers the regexp work it asks for.
    assert_eq!(
        execute_budgeted(&loop_body("/ab+c/.test('xxabbbc')"), 1_000_000)
            .expect("a sufficient budget must complete"),
        ExecutionOutcome::Finished(Value::Number(100.0))
    );
}

/// The charge is exactly the granted allowance over the documented ratio, so
/// two runs of the same program spend the same budget on every replay.
#[test]
fn the_regexp_charge_is_the_documented_ratio() {
    let per_call = lashlang::TYPESCRIPT_REGEXP_EXECUTION_FUEL
        / lashlang::TYPESCRIPT_REGEXP_FUEL_PER_INSTRUCTION;
    let source = "finish(/ab+c/.test('xxabbbc'));";
    // One call costs its charge plus the handful of instructions the cell's own
    // opcodes cost, and cannot cost less than the charge.
    assert!(
        matches!(
            execute_budgeted(source, per_call - 1),
            Err(RuntimeError::InstructionBudgetExceeded { .. })
        ),
        "one regexp call must cost at least the documented charge of {per_call}"
    );
}

/// FIG-3658: a name shared by capture groups in disjoint alternatives is one
/// logical group. A backreference to it consults whichever group participated
/// — matching the empty string when none did — and `groups.name` answers the
/// participating group's text.
#[test]
fn duplicate_named_groups_match_through_whichever_alternative_participated() {
    let cases = [
        (
            "finish(/(?<x>a)|(?<x>b)/.exec('bab').slice(0,3).join('|'));",
            "b||b",
        ),
        (
            "finish(/(?<x>b)|(?<x>a)/.exec('bab').slice(0,3).join('|'));",
            "b|b|",
        ),
        (
            "finish(/(?:(?<x>a)|(?<x>b))\\k<x>/.exec('aa').slice(0,3).join('|'));",
            "aa|a|",
        ),
        (
            "finish(/(?:(?<x>a)|(?<x>b))\\k<x>/.exec('bb').slice(0,3).join('|'));",
            "bb||b",
        ),
        (
            "finish(/(?:(?<x>a)|(?<x>b))\\k<x>/.exec('abab') === null ? 'null' : 'found');",
            "null",
        ),
        (
            "finish(/(?:(?<x>a)|(?<x>b))\\k<x>/.exec('cdef') === null ? 'null' : 'found');",
            "null",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z)\\k<a>$/.exec('xx').slice(0,3).join('|'));",
            "xx|x|",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z)\\k<a>$/.exec('z').slice(0,3).join('|'));",
            "z||",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z)\\k<a>$/.exec('zz') === null ? 'null' : 'found');",
            "null",
        ),
        (
            "finish(/(?<a>x)|(?:zy\\k<a>)/.exec('zy').slice(0,2).join('|'));",
            "zy|",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z){2}\\k<a>$/.exec('xz').slice(0,3).join('|'));",
            "xz||",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z){2}\\k<a>$/.exec('yz').slice(0,3).join('|'));",
            "yz||",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z){2}\\k<a>$/.exec('xzx') === null ? 'null' : 'found');",
            "null",
        ),
        (
            "finish(/^(?:(?<a>x)|(?<a>y)|z){2}\\k<a>$/.exec('yzy') === null ? 'null' : 'found');",
            "null",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
    assert_eq!(
        finished(
            "const m=/(?:(?:(?<x>a)|(?<x>b))\\k<x>){2}/.exec('aabb'); finish([m[0],m[1],m[2],m.groups.x]);"
        ),
        Value::List(
            vec![
                Value::String("aabb".into()),
                Value::Undefined,
                Value::String("b".into()),
                Value::String("b".into()),
            ]
            .into()
        )
    );
    assert!(matches!(
        execute("finish(/(?:(?<x>a)|(?<x>b))\\k<x>/.test('abab'))"),
        Ok(ExecutionOutcome::Finished(Value::Bool(false)))
    ));
}

/// FIG-3658: `(?i:`/`(?-i:` scope ignoreCase to the group — backreferences,
/// `\b`/`\B`, `\w` and `\p` all read the flag in force at their own position —
/// and `(?s:`/`(?-s:`/`(?m:`/`(?-m:` scope `dotAll`/`multiline` the same way.
#[test]
fn regexp_modifiers_scope_flags_to_their_group() {
    // Local `i` folds a backreference's comparison; nothing else changes.
    let re1 = "/(a)(?i:\\1)/";
    for (input, expected) in [("AA", false), ("Aa", false), ("aa", true), ("aA", true)] {
        assert_eq!(
            finished(&format!("finish({re1}.test('{input}'));")),
            Value::Bool(expected),
            "{re1} vs {input}"
        );
    }
    // Local `-i` preserves a case-sensitive backreference under a global `i`.
    let re2 = "/(a)(?-i:\\1)/i";
    for (input, expected) in [("AA", true), ("aA", false), ("Aa", false), ("aa", true)] {
        assert_eq!(
            finished(&format!("finish({re2}.test('{input}'));")),
            Value::Bool(expected),
            "{re2} vs {input}"
        );
    }
    // `\b`/`\B` under local `i` + `u` use the Unicode fold: ſ and K count as
    // word characters inside the group only.
    assert_eq!(
        finished(
            "finish([/(?i:\\b)/u.test('\\u017f'),/(?i:\\b)/u.test('\\u212a'),/(?-i:\\b)/ui.test('\\u017f'),/(?-i:\\b)/ui.test('\\u212a'),/(?i:Z\\B)/u.test('Z\\u017f'),/(?i:Z\\B)/u.test('Z\\u212a'),/(?-i:Z\\B)/ui.test('Z\\u017f'),/(?-i:Z\\B)/ui.test('Z\\u212a'),/(?i:\\w)/u.test('\\u017f'),/(?-i:\\w)/iu.test('\\u017f')]);"
        ),
        Value::List(
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
    // `\p` under local `i` closes over case folds; `\P` negates the raw set
    // first, so both a and A answer.
    assert_eq!(
        finished(
            "finish([/(?i:\\p{Lu})/u.test('a'),/(?i:\\p{Lu})/u.test('\\u03c3'),/(?-i:\\p{Lu})/iu.test('a'),/(?i:\\P{Lu})/u.test('A'),/(?i:\\P{Lu})/u.test('a')]);"
        ),
        Value::List(
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(true),
            ]
            .into()
        )
    );
    // `(?s:` widens `.` inside the group only and `(?m:`/`(?-m:` scope `^`/`$`
    // the same way.
    assert_eq!(
        finished(
            "finish([/(?s:.)/.test('\\n'),/(?m:^b$)/.test('a\\nb'),/(?-m:^b$)/m.test('a\\nb')]);"
        ),
        Value::List(vec![Value::Bool(true), Value::Bool(true), Value::Bool(false)].into())
    );
}

/// FIG-3658: `new RegExp(regexp)` clones the pattern's own source and flags;
/// an explicit flags argument overrides them, and `undefined` flags — spelled
/// or defaulted — inherit the pattern's (ECMA-262 RegExpInitialize).
#[test]
fn regexp_constructor_clones_regexp_patterns() {
    assert_eq!(
        finished(
            "const p=/./i; const r=new RegExp(p); finish([r.source,r.ignoreCase,r.global,r.multiline]);"
        ),
        Value::List(
            vec![
                Value::String(".".into()),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
            ]
            .into()
        )
    );
    assert_eq!(
        finished(
            "const p=/\\t/m; let x; const r=new RegExp(p,x); finish([r.source,r.multiline,r.global]);"
        ),
        Value::List(
            vec![
                Value::String("\\t".into()),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
    assert_eq!(
        finished("const r=new RegExp(new RegExp(),'g'); finish([r.source,r.global,r.ignoreCase]);"),
        Value::List(
            vec![
                Value::String("(?:)".into()),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
    assert_eq!(
        finished("const r=new RegExp(new RegExp('a','gi'),undefined); finish(r.flags);"),
        Value::String("gi".into())
    );
    assert_eq!(
        finished("const p=/a+/; const r=new RegExp(p,'y'); finish([r.source,r.flags]);"),
        Value::List(vec![Value::String("a+".into()), Value::String("y".into())].into())
    );
}

/// FIG-3658: a RegExp instance has no [[Call]], so calling one raises a
/// catchable TypeError — `e instanceof TypeError` must hold in the guest.
#[test]
fn calling_a_regexp_instance_throws_a_catchable_type_error() {
    assert_eq!(
        finished(
            "let verdict='uncaught'; try { /[^a]*/(); } catch (e) { verdict = e instanceof TypeError; } finish(verdict);"
        ),
        Value::Bool(true)
    );
    assert_eq!(
        finished(
            "let name='none'; try { new RegExp('x')(); } catch (e) { name = e.name; } finish(name);"
        ),
        Value::String("TypeError".into())
    );
}

/// FIG-3658: `dotAll` reports the RegExp's own `s` flag — a local `(?-s:`
/// group narrows `.` without touching the flag the property reads.
#[test]
fn regexp_dotall_property_reports_the_outer_flag() {
    assert_eq!(
        finished("finish([/a./s.dotAll,/a./.dotAll,/(?-s:^.$)/s.dotAll,/(?s:.)/.dotAll]);"),
        Value::List(
            vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
}

/// FIG-3658: `match`/`search` run RegExpCreate on a non-RegExp argument —
/// `undefined` (or no argument) is the empty pattern, other primitives coerce
/// through ToString, extra arguments are ignored — and an object whose own
/// `toString`/`valueOf` would answer is refused rather than silently compiled.
#[test]
fn match_and_search_coerce_non_regexp_arguments() {
    assert_eq!(
        finished("finish('gnulluna'.match(null)[0]);"),
        Value::String("null".into())
    );
    assert_eq!(
        finished("finish('gnulluna'.search(null));"),
        Value::Number(1.0)
    );
    assert_eq!(
        finished("finish(String('undefined').search(undefined));"),
        Value::Number(0.0)
    );
    assert_eq!(
        finished("const m='1234567890'.match(3); finish([m[0],m.length,m.index,m.input]);"),
        Value::List(
            vec![
                Value::String("3".into()),
                Value::Number(1.0),
                Value::Number(2.0),
                Value::String("1234567890".into()),
            ]
            .into()
        )
    );
    // Extra arguments are ignored and an absent argument is `undefined`.
    assert_eq!(
        finished("finish(['abc'.search('b','ignored'),'abc'.match().length,'abc'.match()[0]]);"),
        Value::List(
            vec![
                Value::Number(1.0),
                Value::Number(1.0),
                Value::String("".into()),
            ]
            .into()
        )
    );
    // A RegExp argument is used as-is, not recompiled from its source text.
    assert_eq!(
        finished("finish('xBy'.match(/b/i)[0]);"),
        Value::String("B".into())
    );
    // An object whose own methods would answer ToString must refuse: the
    // dialect cannot run guest code inside the coercion.
    let error = execute("finish('AB'.match({ toString: function() { return 'AB'; } }));")
        .expect_err("an object with a guest toString must refuse");
    assert!(
        error.to_string().contains("TS_OBJECT_STRING_COERCION"),
        "the refusal is the named one: {error}"
    );
}

#[test]
fn match_and_search_evaluate_every_argument_in_order() {
    // ECMA-262 evaluates every argument for its side effects even though only
    // the first is used (FIG-3698).
    assert_eq!(
        finished("let i=0; 'a'.match(/a/, i++); finish(i);"),
        Value::Number(1.0)
    );
    assert_eq!(
        finished("let i=0; 'a'.search(/a/, i++); finish(i);"),
        Value::Number(1.0)
    );
    // The extra argument's value is still ignored semantically: the match
    // uses only the first.
    assert_eq!(
        finished("let i=0; finish('ab'.match(/b/, i++)[0]);"),
        Value::String("b".into())
    );
    assert_eq!(
        finished("let i=0; finish('ab'.search(/b/, i++));"),
        Value::Number(1.0)
    );
}

#[test]
fn regexp_constructor_flags_object_coercion_refuses() {
    // `new RegExp(regexp, flags)` must apply the same guest-coercion guard to
    // flags as the string-coercion paths do (FIG-3698).
    let error = execute(
        "const p=/a/g; const f={toString:function(){return 'i';}}; finish(new RegExp(p, f).flags);",
    )
    .expect_err("an object with a guest toString must refuse as flags");
    assert!(
        error.to_string().contains("TS_OBJECT_STRING_COERCION"),
        "the refusal is the named one: {error}"
    );
    // String and absent flags still work.
    assert_eq!(
        finished("const p=/a/g; finish(new RegExp(p, 'i').flags);"),
        Value::String("i".into())
    );
    assert_eq!(
        finished("const p=/a/g; finish(new RegExp(p).flags);"),
        Value::String("g".into())
    );
}

/// FIG-3704: `RegExp(pattern, flags)` called as a function constructs as
/// `new RegExp` would, and returns `pattern` itself when it is a RegExp and
/// `flags` is `undefined` (ECMA-262 `RegExp()` — no subclassing exists, so a
/// RegExp's constructor is always `RegExp`).
#[test]
fn regexp_called_without_new_constructs_or_returns_the_pattern() {
    assert_eq!(
        finished("const r=RegExp('a+','i'); finish([r.source,r.flags,r.ignoreCase]);"),
        Value::List(
            vec![
                Value::String("a+".into()),
                Value::String("i".into()),
                Value::Bool(true),
            ]
            .into()
        )
    );
    assert_eq!(
        finished("finish([RegExp().source,RegExp(undefined,'g').source]);"),
        Value::List(vec![Value::String("(?:)".into()), Value::String("(?:)".into())].into())
    );
    assert_eq!(
        finished("const re=/x/i; finish([RegExp(re)===re,RegExp(re,undefined)===re]);"),
        Value::List(vec![Value::Bool(true), Value::Bool(true)].into())
    );
    assert_eq!(
        finished("const re=/x/i; const r=RegExp(re,'g'); finish([r===re,r.source,r.flags]);"),
        Value::List(
            vec![
                Value::Bool(false),
                Value::String("x".into()),
                Value::String("g".into()),
            ]
            .into()
        )
    );
    assert_eq!(
        finished("finish(RegExp(/y+/m).flags);"),
        Value::String("m".into())
    );
    assert_eq!(
        finished(
            "let verdict='uncaught'; try { RegExp('\\\\'); } catch (e) { verdict = e instanceof SyntaxError; } finish(verdict);"
        ),
        Value::Bool(true)
    );
}
