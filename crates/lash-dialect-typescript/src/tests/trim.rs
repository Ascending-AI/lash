//! ECMA-262 TrimString: the native scan and the interpreted UTF-16 oracle.
use std::sync::Arc;

use lash_kernel_dialect::{NamedLibrary, define_functions};
use lash_kernel_doc::{Datum, Integer, Name, Timestamp, parse_document};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, PreparedLibrary, Program, Start, Step,
    Target,
};

// Independent pre-optimization TrimString oracle; never used in production.
const REFERENCE: &str = r#"
use num.lt
use num.sub
use num.add
use list.contains
use text.utf16_len
use text.utf16_get
use text.utf16_slice
use ts.law.is_space
# Whether a code unit is ECMA WhiteSpace or a LineTerminator: BOM is
# included, NEL is not. Every such unit is below 33 or above 159.
function ts.law.is_space(unit: Int) -> Bool
kernel 1
charge 4
body {
  if num.lt(32, unit) { if num.lt(unit, 160) { return false } }
  return list.contains([9, 10, 11, 12, 13, 32, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288, 65279], unit)
}

# Trims ECMA white space from either end.
function ts.law.trim_value(text: Text, leading: Bool, trailing: Bool) -> Text
kernel 1
errors "type_error", "RangeError", "TS_LONE_SURROGATE_UNSUPPORTED"
charge sum(8, size(text))
body {
  let start = 0
  let end = text.utf16_len(text)
  if leading {
    while num.lt(start, end) {
      let unit = text.utf16_get(text, start)
      let space = invoke ts.law.is_space(unit)
      if space {} else { break }
      set start = num.add(start, 1)
    }
  }
  if trailing {
    while num.lt(start, end) {
      let unit = text.utf16_get(text, num.sub(end, 1))
      let space = invoke ts.law.is_space(unit)
      if space {} else { break }
      set end = num.sub(end, 1)
    }
  }
  return text.utf16_slice(text, start, end)
}

"#;

#[derive(Default)]
struct Console(Vec<Datum>);
impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(
        &mut self,
        _: &lash_kernel_doc::Handle,
        _: &Datum,
    ) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
        unreachable!("trim does not read")
    }
    fn print(&mut self, value: &Datum) {
        self.0.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// ECMA-262 TrimString across every whitespace member, nonmembers and generated
/// scalar strings: identical results/prints, no parks or errors, and the native
/// charge bounds its new work rather than freezing the old interpreted charge.
#[test]
fn native_trim_agrees_with_generic_utf16_scan() {
    let mut registry = super::kernel_registry();
    let mut named = NamedLibrary::from_registry(&registry).unwrap();
    for definition in crate::define_helpers(&mut named).unwrap() {
        registry.register(definition, None).unwrap();
    }
    for definition in define_functions(REFERENCE, &mut named).unwrap() {
        registry.register(definition, None).unwrap();
    }
    let mut header = String::from("kernel 1\nnumbers by_spelling\n");
    for (name, id) in named.iter() {
        header.push_str(&format!("use {name} = @{id}\n"));
    }
    let prepared = PreparedLibrary::new(Arc::new(registry));
    let make_program = |helper: &str| {
        let document = parse_document(&format!("{header}entry trim(text: Text, leading: Bool, trailing: Bool) -> Text\nfn trim(text, leading, trailing) {{ let output = invoke {helper}(text, leading, trailing) print output return output }} main {{}}")).unwrap();
        Program {
            document: Arc::new(document),
            library: prepared.clone(),
        }
    };
    let fast = make_program("ts.string.trim_value");
    let generic = make_program("ts.law.trim_value");
    let bounds = Bounds {
        charge: 10_000_000,
        memory: 1 << 20,
        call_depth: 100,
        live_tasks: 10,
        requests_per_park: 10,
        join_members: 10,
    };
    let observe = |program: &Program, args: Vec<Datum>| {
        let mut machine = KernelMachine::start(
            program.clone(),
            bounds,
            Start {
                target: Target::Entry(Name::new("trim")),
                args,
                bindings: Bindings::default(),
            },
        )
        .unwrap();
        let mut console = Console::default();
        match machine.run(&mut console, u64::MAX).unwrap() {
            Step::Ended(End::Finished(finished)) => {
                (finished.result, console.0, machine.meters().charged)
            }
            other => panic!("trim must return without parking: {other:?}"),
        }
    };
    let whitespace = [
        9, 10, 11, 12, 13, 32, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200,
        8201, 8202, 8232, 8233, 8239, 8287, 12288, 65279,
    ];
    let mut alphabet: Vec<char> = whitespace
        .into_iter()
        .map(|unit| char::from_u32(unit).unwrap())
        .collect();
    alphabet.extend([
        'x', '😀', '\u{85}', '\u{180e}', '\u{200b}', '\u{2060}', '\0',
    ]);
    let mut inputs = vec![String::new(), alphabet[..25].iter().collect()];
    for character in &alphabet {
        inputs.push(format!("{character}x😀{character}"));
        inputs.push(character.to_string().repeat(4));
    }
    // Bounded generated values; Smith currently generates programs rather than
    // arbitrary text arguments, so this scalar grammar targets the trim traps.
    for seed in 0..128usize {
        let mut state = seed + 1;
        let mut text = String::new();
        for _ in 0..seed % 32 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            text.push(alphabet[state % alphabet.len()]);
        }
        inputs.push(text);
    }
    for text in inputs {
        for (leading, trailing) in [(false, false), (true, false), (false, true), (true, true)] {
            let args = vec![
                Datum::Text(text.clone()),
                Datum::Bool(leading),
                Datum::Bool(trailing),
            ];
            let (result, printed, charged) = observe(&fast, args.clone());
            let (reference, reference_printed, _) = observe(&generic, args);
            assert_eq!(result, reference, "{text:?} {leading}/{trailing}");
            assert_eq!(printed, reference_printed);
            let Datum::Text(result) = result else {
                unreachable!()
            };
            assert!(charged >= 8 + (1 + text.len() as u64) * 62 + result.len() as u64);
        }
    }
}

/// ECMA-262 RequireObjectCoercible precedes ToString; a missed primitive guard
/// converts once, preserves thrown values and prints, and cannot split pairs.
#[test]
fn trim_guard_keeps_receiver_coercion_errors_and_prints() {
    super::remaining_builtins::agrees(r#"
        let calls = 0; let errors = 0;
        const receiver = {toString() { calls++; return '\uFEFF 😀 x \u3000'; }};
        const a = String.prototype.trim.call(receiver);
        const b = String.prototype.trimStart.call(receiver);
        const c = String.prototype.trimEnd.call(receiver);
        for (const value of [null, undefined]) {
            try { String.prototype.trim.call(value); } catch (e) { if (e.name === 'TypeError') errors++; }
        }
        const throwing = {toString() { console.log('coerce'); throw 17; }};
        let thrown = false;
        try { String.prototype.trim.call(throwing); } catch (e) { thrown = e === 17; }
        await finish(calls === 3 && errors === 2 && thrown && a === '😀 x' && b === '😀 x \u3000' && c === '\uFEFF 😀 x' && String.prototype.trim.call(42) === '42');
    "#.trim());
    let recorded = super::machine::run(
        "const receiver = {toString() { console.log('coerce'); throw 17; }}; try { String.prototype.trim.call(receiver); } catch (e) { console.log(e); }",
        &[],
    );
    assert_eq!(recorded.lines(), ["coerce", "17"]);
    assert_eq!(recorded.end, "ok");
}
