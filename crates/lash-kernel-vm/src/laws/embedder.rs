//! A minimal embedder, in memory: it loads a document from kernel text,
//! runs it, answers its effects from a table and delivers the outcomes in
//! the order a law chooses. It uses the crate's public interface only.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, ErrorValue, FunctionId, FunctionRegistry, Handle, Integer, NativeCall,
    NativeError, NativeFunction, NumberToken, Object, Site, Timestamp, Unit, Value,
    parse_definition, parse_document,
};

use crate::{
    Bounds, Delivered, End, Host, KernelMachine, Layout, Machine, Outcome, Program, Request,
    RunError, Start, Step, Target, WaitId, register_machine_functions,
};

/// Bounds no law reaches unless it sets one.
pub(crate) const ROOMY: Bounds = Bounds {
    charge: 10_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

fn type_error(message: &str) -> NativeError {
    NativeError::Raised(ErrorValue::new("type_error", message))
}

struct Add;
impl NativeFunction for Add {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        match call.args {
            [Value::Int(a), Value::Int(b)] => {
                Ok(Value::Int(Integer::new(a.as_bigint() + b.as_bigint())))
            }
            _ => Err(type_error("num.add takes two integers")),
        }
    }
}

struct Less;
impl NativeFunction for Less {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        match call.args {
            [Value::Int(a), Value::Int(b)] => Ok(Value::Bool(a < b)),
            _ => Err(type_error("num.lt takes two integers")),
        }
    }
}

struct Concat;
impl NativeFunction for Concat {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        match call.args {
            [Value::Text(a), Value::Text(b)] => Ok(Value::text(format!("{a}{b}"))),
            _ => Err(type_error("text.concat takes two texts")),
        }
    }
}

struct Len;
impl NativeFunction for Len {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        match call.args.first().and_then(Value::object) {
            Some(object) => Ok(Value::Int(Integer::from(call.heap.len(object) as i64))),
            None => Err(type_error("list.len takes a collection")),
        }
    }
}

/// Spends one unit of its guard per step.
struct Spin;
impl NativeFunction for Spin {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let [Value::Int(steps)] = call.args else {
            return Err(type_error("work.spin takes an integer"));
        };
        let steps = u64::try_from(steps.as_bigint()).unwrap_or(0);
        for _ in 0..steps {
            call.counter.spend(1)?;
        }
        Ok(call.args[0].clone())
    }
}

/// Allocates a list of that many nulls.
struct Fill;
impl NativeFunction for Fill {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let [Value::Int(length)] = call.args else {
            return Err(type_error("work.fill takes an integer"));
        };
        let length = usize::try_from(length.as_bigint()).unwrap_or(0);
        call.heap
            .allocate(Object::List(vec![Value::Null; length]))
            .map(Value::List)
    }
}

struct Ref;
impl NativeFunction for Ref {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        match call.args.first().and_then(Value::object) {
            Some(object) => Ok(Value::Ref(lash_kernel_doc::Identity::Object(object))),
            None => Err(type_error("ident.ref takes an object")),
        }
    }
}

struct Twice;
impl NativeFunction for Twice {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        Add.call(NativeCall {
            args: &[call.args[0].clone(), call.args[0].clone()],
            heap: call.heap,
            counter: call.counter,
        })
    }
}

/// The handful of library functions the laws call, and the header that
/// names them in a document.
pub(crate) struct Library {
    pub(crate) registry: Arc<FunctionRegistry>,
    pub(crate) ids: BTreeMap<&'static str, FunctionId>,
    header: String,
}

/// Builds the laws' library. `pair.twice` has a native implementation and
/// a kernel body; `native_twice` says whether the native one is registered.
pub(crate) fn library(native_twice: bool) -> Library {
    let mut registry = FunctionRegistry::new();
    let mut ids = BTreeMap::new();
    let machine = register_machine_functions(&mut registry).unwrap();
    ids.insert("deref", machine.deref);
    ids.insert("tasks.unfinished", machine.tasks_unfinished);
    let natives: [(&'static str, &str, Arc<dyn NativeFunction>); 7] = [
        (
            "num.add",
            "(a: Any, b: Any) -> Any\nkernel 1\ncharge 1",
            Arc::new(Add),
        ),
        (
            "num.lt",
            "(a: Any, b: Any) -> Bool\nkernel 1\ncharge 1",
            Arc::new(Less),
        ),
        (
            "text.concat",
            "(a: Any, b: Any) -> Text\nkernel 1\ncharge size(result)",
            Arc::new(Concat),
        ),
        (
            "list.len",
            "(xs: Any) -> Int\nkernel 1\ncharge 1",
            Arc::new(Len),
        ),
        (
            "work.spin",
            "(steps: Any) -> Any\nkernel 1\ncharge 1\nguard \"step\" 100",
            Arc::new(Spin),
        ),
        (
            "work.fill",
            "(length: Any) -> Any\nkernel 1\ncharge 1",
            Arc::new(Fill),
        ),
        (
            "ident.ref",
            "(x: Any) -> Any\nkernel 1\ncharge 1",
            Arc::new(Ref),
        ),
    ];
    for (name, rest, native) in natives {
        let definition = parse_definition(&format!("function {name}{rest}\nnative\n")).unwrap();
        ids.insert(name, registry.register(definition, Some(native)).unwrap());
    }
    let twice = parse_definition(&format!(
        "function pair.twice(x: Any) -> Any\nkernel 1\ncharge 7\nuse num.add = @{}\nnative\n\
         body {{ return num.add(x, x) }}\n",
        ids["num.add"]
    ))
    .unwrap();
    let native: Option<Arc<dyn NativeFunction>> = native_twice.then(|| Arc::new(Twice) as _);
    ids.insert("pair.twice", registry.register(twice, native).unwrap());
    let each = parse_definition(
        "function each.twice(f: Fn(x: Any) -> Any, x: Any) -> Any\nkernel 1\ncharge 5\n\
         body { let a = apply f(x) let b = apply f(a) return b }\n",
    )
    .unwrap();
    ids.insert("each.twice", registry.register(each, None).unwrap());
    let mut header = String::from(
        "kernel 1\neffect echo(x?: Any) -> Any\neffect boom(x?: Any) -> Any\n\
         effect num(x: Text) -> Any\n",
    );
    for (name, id) in &ids {
        header.push_str(&format!("use {name} = @{id}\n"));
    }
    Library {
        registry: Arc::new(registry),
        ids,
        header,
    }
}

/// The world outside the run: a clock, random bits, one projection, and
/// what the run printed and read.
#[derive(Default)]
pub(crate) struct World {
    pub(crate) printed: Vec<Datum>,
    pub(crate) reads: Vec<Datum>,
    pub(crate) now: i64,
    pub(crate) random: u64,
    pub(crate) cancel: bool,
}

impl Host for World {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(self.now),
        }
    }

    fn random(&mut self) -> u64 {
        self.random
    }

    fn read(&mut self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        self.reads.push(request.clone());
        if handle.kind == "broken" {
            return Err(ErrorDatum {
                kind: "host".to_string(),
                message: "the projection is gone".to_string(),
                data: Datum::Null,
            });
        }
        Ok(match request {
            Datum::Text(text) if text == "index" => int(0),
            other => other.clone(),
        })
    }

    fn print(&mut self, value: &Datum) {
        self.printed.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        self.cancel
    }
}

/// How a law starts a run.
pub(crate) struct Setup {
    pub(crate) bounds: Bounds,
    pub(crate) layout: Layout,
    pub(crate) numbers: &'static str,
    pub(crate) native_twice: bool,
    pub(crate) start: Start,
    pub(crate) slice: u64,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            bounds: ROOMY,
            layout: Layout::default(),
            numbers: "by_spelling",
            native_twice: true,
            start: Start {
                target: Target::Main,
                args: Vec::new(),
                bindings: Default::default(),
            },
            slice: u64::MAX,
        }
    }
}

pub(crate) struct Embedder {
    pub(crate) machine: KernelMachine,
    pub(crate) world: World,
    pub(crate) library: Library,
    /// Every request the machine handed out, in order.
    pub(crate) requests: Vec<Request>,
    /// The requests handed out and not yet answered.
    pub(crate) pending: Vec<Request>,
    pub(crate) withdrawn: Vec<WaitId>,
    slice: u64,
}

fn wait_of(request: &Request) -> WaitId {
    match request {
        Request::Effect(effect) => effect.wait,
        Request::Sleep(sleep) => sleep.wait,
    }
}

/// What a law calls a request by: an effect's first argument when it is a
/// text, else the effect's name; `sleep` for a sleep.
pub(crate) fn label(request: &Request) -> String {
    match request {
        Request::Effect(effect) => match effect.args.first() {
            Some(Datum::Text(text)) => text.clone(),
            _ => effect.effect.to_string(),
        },
        Request::Sleep(_) => "sleep".to_string(),
    }
}

/// The table effects are answered from.
fn answer(request: &Request) -> Outcome {
    let Request::Effect(effect) = request else {
        return Outcome::Elapsed;
    };
    let first = effect.args.first().cloned().unwrap_or(Datum::Null);
    match effect.effect.as_str() {
        "echo" => Outcome::Completed(first),
        "num" => match first {
            Datum::Text(text) => Outcome::Completed(Datum::Number(NumberToken::new(text).unwrap())),
            other => Outcome::Completed(other),
        },
        _ => Outcome::Failed(ErrorDatum {
            kind: "boom".to_string(),
            message: label(request),
            data: first,
        }),
    }
}

impl Embedder {
    /// Loads a document: the laws' header, then `text`.
    pub(crate) fn new(text: &str) -> Self {
        Self::with(text, Setup::default())
    }

    pub(crate) fn with(text: &str, setup: Setup) -> Self {
        let library = library(setup.native_twice);
        let text = format!("numbers {}\n{}{text}", setup.numbers, library.header);
        let document = parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        let program = Program {
            document: Arc::new(document),
            registry: Arc::clone(&library.registry),
        };
        let machine =
            KernelMachine::start_with_layout(program, setup.bounds, setup.start, setup.layout)
                .unwrap_or_else(|error| panic!("{error}\n{text}"));
        Self {
            machine,
            world: World::default(),
            library,
            requests: Vec::new(),
            pending: Vec::new(),
            withdrawn: Vec::new(),
            slice: setup.slice,
        }
    }

    /// Runs the machine once and keeps what a park hands out.
    pub(crate) fn run(&mut self) -> Step {
        let step = self.machine.run(&mut self.world, self.slice).unwrap();
        if let Step::Parked(park) = &step {
            self.requests.extend(park.requests.iter().cloned());
            self.pending.extend(park.requests.iter().cloned());
            self.withdrawn.extend(park.withdrawn.iter().copied());
        }
        step
    }

    /// Answers the pending request a law calls `name` from the table.
    pub(crate) fn deliver(&mut self, name: &str) -> Delivered {
        let index = self
            .pending
            .iter()
            .position(|request| label(request) == name)
            .unwrap_or_else(|| panic!("no pending request `{name}`"));
        let request = self.pending.remove(index);
        self.machine
            .deliver(wait_of(&request), answer(&request))
            .unwrap()
    }

    /// Runs to the end. At each park it delivers the next request `order`
    /// names, or the oldest pending one once `order` is used up.
    pub(crate) fn run_to_end(&mut self, order: &[&str]) -> End {
        let mut order = order.iter();
        loop {
            match self.run() {
                Step::Ended(end) => return end,
                Step::Slice => {}
                Step::Parked(_) => {
                    let next = match order.next() {
                        Some(name) => (*name).to_string(),
                        None => label(self.pending.first().expect("a park with nothing pending")),
                    };
                    self.deliver(&next);
                }
            }
        }
    }

    /// The labels of the requests handed out so far.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.requests.iter().map(label).collect()
    }
}

/// Runs a document to its end, answering effects oldest first.
pub(crate) fn run(text: &str) -> (End, Embedder) {
    let mut embedder = Embedder::new(text);
    let end = embedder.run_to_end(&[]);
    (end, embedder)
}

/// The result of a run that finished.
pub(crate) fn result(end: End) -> Datum {
    match end {
        End::Finished(finished) => finished.result,
        other => panic!("the run did not finish: {other:?}"),
    }
}

/// The result of running `main`'s body to its end.
pub(crate) fn value(body: &str) -> Datum {
    result(run(&format!("main {{ {body} }}")).0)
}

/// The kind of the error `main`'s body ends in, uncaught.
pub(crate) fn uncaught(body: &str) -> String {
    match run(&format!("main {{ {body} }}")).0 {
        End::Error(RunError::Uncaught(Datum::Error(error))) => error.kind,
        other => panic!("the run did not end in an uncaught error: {other:?}"),
    }
}

pub(crate) fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

pub(crate) fn text(value: &str) -> Datum {
    Datum::Text(value.to_string())
}

pub(crate) fn record<const N: usize>(fields: [(&str, Datum); N]) -> Datum {
    Datum::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

pub(crate) fn main_site<const N: usize>(path: [u32; N]) -> Site {
    Site::new(Unit::Main, path)
}

pub(crate) fn function_site<const N: usize>(name: &str, path: [u32; N]) -> Site {
    Site::new(Unit::Function(name.into()), path)
}
