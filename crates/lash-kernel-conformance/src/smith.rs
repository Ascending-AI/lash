//! Bounded, dialect-neutral programs for differential execution (SC-DESIGN §9.2).
//!
//! The grammar builds scoped, typed fragments instead of filtering arbitrary
//! syntax through admission. Counts, collection widths and loop trips are
//! bounded independently; the run also has finite charge, memory and step
//! bounds. Runtime failures are intentional, admitted programs too.

use std::sync::{Arc, OnceLock};

use arbitrary::{Arbitrary, Unstructured};
use lash_kernel_doc::{Document, FunctionRegistry, Name, parse_document};
use lash_kernel_vm::{Bounds, PreparedLibrary};

mod runner;
pub use runner::{HostCall, Observation, check_interpreter, compare};

/// A program and its environment script. The bytes also choose resource
/// bounds, delivery order, slices and points at which the executable is lost.
#[derive(Clone, Debug)]
pub struct Smith {
    pub document: Document,
    pub schedule: Schedule,
}

/// Choices applied identically to both implementations, except for slice
/// size and rebuilding. A delivery always belongs to its request identity.
#[derive(Clone, Debug)]
pub struct Schedule {
    pub slices: Vec<u64>,
    pub rebuild: Vec<bool>,
    pub reverse_deliveries: bool,
    pub fail_effects: bool,
    pub host_seed: u64,
    pub bounds: Bounds,
}

pub const MAX_FRAGMENTS: u8 = 12;
pub const MAX_WIDTH: u8 = 16;
pub const MAX_TRIPS: u8 = 8;
const FRAGMENT_KINDS: u8 = 22;

struct Library {
    registry: Arc<FunctionRegistry>,
    prepared: PreparedLibrary,
    header: String,
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let mut registry = FunctionRegistry::new();
        lash_kernel_vm::register_machine_functions(&mut registry)
            .unwrap_or_else(|error| panic!("machine definitions: {error}"));
        lash_kernel_lib::register_numbers(&mut registry)
            .unwrap_or_else(|error| panic!("numeric definitions: {error}"));
        lash_kernel_lib::register_text_json(&mut registry)
            .unwrap_or_else(|error| panic!("text definitions: {error}"));
        lash_kernel_lib::register_collections(&mut registry)
            .unwrap_or_else(|error| panic!("collection bodies: {error}"));
        let mut header =
            String::from("kernel 1\nnumbers by_spelling\neffect echo(x: Any) -> Any\n");
        for (id, function) in registry.iter() {
            header.push_str(&format!("use {} = @{id}\n", function.definition.name));
        }
        let registry = Arc::new(registry);
        Library {
            prepared: PreparedLibrary::new(Arc::clone(&registry)),
            registry,
            header,
        }
    })
}

impl Smith {
    /// Admission is exposed separately so a generator defect is distinguished
    /// from an executor defect. Projection handles are supplied at start.
    pub fn admit(&self) -> Result<lash_kernel_check::Admitted, lash_kernel_check::Refusal> {
        let mut env = lash_kernel_check::Environment::new(library().registry.as_ref());
        env.effects = self.document.manifest.effects.clone();
        env.bindings.insert(Name::new("projection"));
        lash_kernel_check::admit(&self.document, &env)
    }
}

impl<'a> Arbitrary<'a> for Smith {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let count = u.int_in_range(1..=MAX_FRAGMENTS)?;
        let mut text = library().header.clone();
        // Real declared calls, waits in cleanup, and shared-cell closures are
        // available to every fragment. These definitions are themselves bounded.
        text.push_str(
            "fn worker(x) { let y = perform echo(x) as Any do sleep 1 return y }\n\
             fn quick(x) { return x }\n\
             fn depart(mode) { try {\n\
             if eq(mode, 0) { return 7 }\n\
             if eq(mode, 1) { throw 9 }\n\
             if eq(mode, 2) { finish 11 }\n\
             if eq(mode, 3) { fail 13 }\n\
             return 17\n\
             } catch e { return e } finally { do sleep 1 print \"finally\" } }\n\
             main { let acc = 0\n",
        );
        for _ in 0..count {
            let kind = u.int_in_range(0..=FRAGMENT_KINDS - 1)?;
            let width = u.int_in_range(1..=MAX_WIDTH)?;
            let trips = u.int_in_range(1..=MAX_TRIPS)?;
            let value = u.arbitrary::<i16>()?;
            let mode = u.int_in_range(0..=4u8)?;
            text.push_str("if true {\n");
            text.push_str(&fragment(kind, width, trips, value, mode));
            text.push_str("\n}\n");
        }
        let ending = u.int_in_range(0..=3u8)?;
        text.push_str(match ending {
            0 => "return acc\n}",
            1 => "finish acc\n}",
            2 => "fail acc\n}",
            _ => "throw acc\n}",
        });
        let mut document = parse_document(&text)
            .unwrap_or_else(|error| panic!("smith grammar must parse: {error}"));
        // `use` resolves textual names; only the reachable identities belong
        // in the final manifest (including the dependencies of real bodies).
        let requirements = lash_kernel_check::requirements(&document, library().registry.as_ref());
        document.manifest.functions = requirements.functions;
        document
            .manifest
            .effects
            .retain(|name, _| requirements.effects.contains(name));
        let choices = u.int_in_range(1..=8u8)?;
        let mut slices = Vec::new();
        let mut rebuild = Vec::new();
        for _ in 0..choices {
            slices.push(u.int_in_range(1..=64u64)?);
            rebuild.push(u.arbitrary()?);
        }
        let schedule = Schedule {
            slices,
            rebuild,
            reverse_deliveries: u.arbitrary()?,
            fail_effects: u.arbitrary()?,
            host_seed: u.arbitrary()?,
            bounds: Bounds {
                charge: u.int_in_range(0..=20_000)?,
                memory: u.int_in_range(0..=256 * 1024)?,
                call_depth: u.int_in_range(1..=16)?,
                live_tasks: u.int_in_range(1..=8)?,
                requests_per_park: u.int_in_range(1..=16)?,
                join_members: u.int_in_range(1..=8)?,
            },
        };
        Ok(Self { document, schedule })
    }
}

// Each fragment is closed except for `acc` and the supplied projection.
// All action arguments are atoms, and each loop has a finite trip guard.
fn fragment(kind: u8, width: u8, trips: u8, value: i16, mode: u8) -> String {
    let xs = (0..width)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match kind {
        0 => format!("print (null, absent, true, {value}, 1.5, \"λ\", b\"616263\", &quick)"),
        1 => format!("let xs = [{xs}] let alias = xs set xs[0] = {value} print alias[0] remove xs[0] print xs"),
        2 => format!("let r = {{a: {value}, b: [{xs}]}} set r.a = 3 print r.a remove r.b print r"),
        3 => format!("let m = map{{1: {value}, 2: 9}} for k in m {{ set m[3] = 5 remove m[2] print k }} print m"),
        4 => "let s = set{1, 2, 3} for k in s { remove s[2] set s[4] = true print k } print s".into(),
        5 => format!("for x in ({xs},) {{ print x }}"),
        6 => format!("let xs = [{xs}] for x in xs {{ set xs[0] = {value} print x }}"),
        7 => format!("let i = 0 while num.lt(i, {trips}) {{ set i = num.add(i, 1) try {{ if eq(i, 1) {{ continue }} if eq(i, {trips}) {{ break }} print i }} finally {{ print i }} }}"),
        8 => format!("let shared = {value} let f = fn(x) {{ set shared = num.add(shared, x) return shared }} let a = apply f(1) let b = apply f(2) set acc = num.add(acc, b) print (a, b)"),
        9 => format!("let x = call quick({value}) print x"),
        10 => format!("try {{ let x = call worker({value}) print x }} catch e {{ print e }}"),
        11 => "do sleep 1 do yield".into(),
        12 => format!("let a = spawn call worker({value}) let b = spawn call worker(2) let hs = [a, b] try {{ let rs = join {} hs print rs }} catch e {{ print e }}", ["all", "settled", "race", "any", "all"][usize::from(mode)]),
        13 => format!("let h = spawn call worker({value}) do yield do cancel h try {{ let r = join h print r }} catch e {{ print e.kind }}"),
        14 => format!("try {{ throw [{value}, \"error\"] }} catch e {{ print e }} finally {{ do sleep 1 }}"),
        15 => format!("let x = call depart({mode}) print x"),
        16 => format!("let text = text.repeat(\"abcdλ\", {}) print json.stringify({{text: text, xs: [{xs}]}})", u16::from(width) * u16::from(trips)),
        17 => format!("let xs = [{xs}] let f = fn(x) {{ set acc = num.add(acc, 1) let y = perform echo(x) as Any return y }} try {{ let ys = invoke collection.map(xs, f) print ys }} catch e {{ print e }}"),
        18 => format!("let xs = [{xs}] do invoke list.insert(xs, 0, {value}) let old = invoke list.pop(xs) print old do invoke list.clear(xs) print xs"),
        19 => "print clock print random try { print read(projection, \"request\") } catch e { print e }".into(),
        20 => "let f = &quick let h = spawn apply f(7) let joined = join h print joined".into(),
        // Admitted error-producing expressions, including native errors.
        _ => format!("try {{ print num.div({value}, 0) }} catch e {{ print e.kind }} try {{ print [1][9] }} catch e {{ print e.kind }}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// K-ADM-001 and SC-DESIGN §9.2: generation constructs admitted
    /// documents rather than silently discarding ill-typed inputs.
    #[test]
    fn generated_documents_pass_the_kernel_checker() {
        let mut executed = 0;
        for seed in 0..256u64 {
            let mut state = seed + 1;
            let bytes: Vec<u8> = (0..512)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            let smith = Smith::arbitrary(&mut Unstructured::new(&bytes))
                .unwrap_or_else(|error| panic!("bounded generation: {error}"));
            smith
                .admit()
                .unwrap_or_else(|error| panic!("seed {seed}: {error}\n{:?}", smith.document));
            executed += 1;
        }
        println!("kernel-smith seeded checker: executed={executed}");
    }
}
