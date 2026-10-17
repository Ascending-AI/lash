//! A minimal embedder, in memory: it loads a document from kernel text,
//! runs it, answers its effects from a table and delivers the outcomes in
//! the order a law chooses. It uses the crate's public interface only.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_kernel_doc::heap::*;
use lash_kernel_doc::{
    Datum, Document, ErrorDatum, FunctionId, FunctionRegistry, Handle, Integer, NativeCall,
    NativeError, NativeFunction, NumberToken, Object, PreparedNative, Site, Timestamp, Unit, Value,
    parse_definition, parse_document,
};
use lash_kernel_state::paged::*;
use num_traits::ToPrimitive;
use std::cell::RefCell;
use std::rc::Rc;

use crate::{
    Bounds, CellStart, Delivered, End, Host, KernelMachine, Layout, Machine, Outcome,
    PreparedLibrary, Program, Request, RunError, Step, Target, WaitId, register_machine_functions,
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
    NativeError::Raised(Arc::new(ErrorRef::new("type_error", message)))
}

/// Heap-independent law natives offer a prepared entry with the same answer.
macro_rules! prepared {
    ($native:ident, |$args:ident| $body:expr) => {
        impl $native {
            fn answer($args: &[Value]) -> Result<Value, NativeError> {
                $body
            }
        }
        impl NativeFunction for $native {
            fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
                Self::answer(call.args)
            }
            fn prepare(&self) -> Option<PreparedNative> {
                Some(PreparedNative {
                    entry: |args| Self::answer(args).ok(),
                    operation: stringify!($native),
                })
            }
        }
    };
}

struct Add;
prepared!(Add, |args| match args {
    [Value::Int(IntRef::Inline(a)), Value::Int(IntRef::Inline(b))] => Ok(Value::Int(
        IntRef::Inline(Integer::new(&*a.as_bigint() + &*b.as_bigint()))
    )),
    _ => Err(type_error("num.add takes two integers")),
});

struct Less;
prepared!(Less, |args| match args {
    [Value::Int(IntRef::Inline(a)), Value::Int(IntRef::Inline(b))] => Ok(Value::Bool(a < b)),
    _ => Err(type_error("num.lt takes two integers")),
});

struct Concat;
prepared!(Concat, |args| match args {
    [
        Value::Text(TextRef {
            body: Payload::Resident(a),
            ..
        }),
        Value::Text(TextRef {
            body: Payload::Resident(b),
            ..
        }),
    ] => Ok(Value::text(format!("{a}{b}"))),
    _ => Err(type_error("text.concat takes two texts")),
});

struct Len;
impl NativeFunction for Len {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        match call.args.first().and_then(Value::object) {
            Some(object) => Ok(Value::Int(call.heap.len(object)?.into())),
            None => Err(type_error("list.len takes a collection")),
        }
    }
}

/// Spends one unit of its guard per step.
struct Spin;
impl NativeFunction for Spin {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let [Value::Int(IntRef::Inline(steps))] = call.args else {
            return Err(type_error("work.spin takes an integer"));
        };
        let steps = steps.to_u64().unwrap_or(0);
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
        let [Value::Int(IntRef::Inline(length))] = call.args else {
            return Err(type_error("work.fill takes an integer"));
        };
        let length = length.to_usize().unwrap_or(0);
        call.heap
            .allocate(Object::List(vec![Value::Null; length]))
            .map(Value::List)
    }
}

/// How many texts `work.repeat` has built.
pub(crate) static REPEAT_BUILDS: AtomicUsize = AtomicUsize::new(0);

/// Reserves a text of that many bytes, then builds it.
struct Repeat;
impl NativeFunction for Repeat {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let [Value::Int(IntRef::Inline(length))] = call.args else {
            return Err(type_error("work.repeat takes an integer"));
        };
        let length = length.to_u64().unwrap_or(0);
        call.heap.reserve(0, length)?;
        REPEAT_BUILDS.fetch_add(1, Ordering::Relaxed);
        Ok(Value::text("x".repeat(length as usize)))
    }
}

struct Ref;
prepared!(Ref, |args| match args.first().and_then(Value::object) {
    Some(object) => Ok(Value::Ref(lash_kernel_doc::Identity::Object(object))),
    None => Err(type_error("ident.ref takes an object")),
});

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
struct LargeError;
impl NativeFunction for LargeError {
    fn call(&self, _call: NativeCall<'_>) -> Result<Value, NativeError> {
        Err(NativeError::Raised(Arc::new(ErrorRef::new(
            &"k".repeat(1 << 20),
            &"m".repeat(1 << 20),
        ))))
    }
}

pub(crate) struct Library {
    pub(crate) registry: Arc<FunctionRegistry>,
    pub(crate) ids: BTreeMap<&'static str, FunctionId>,
    pub(crate) header: String,
}

/// Builds the laws' library. `pair.twice` has a native implementation and
/// a kernel body; `native_twice` says whether the native one is registered.
/// Its formula reads its argument: 7 for a one-word integer.
pub(crate) fn library(native_twice: bool) -> Library {
    library_with(native_twice, None)
}

/// Library functions a law adds to the laws' library, each with the name
/// the header gives it: registered after the library's own, which they
/// may use.
pub(crate) type Probes = fn(
    &mut FunctionRegistry,
    &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)>;

/// [`library`] with the functions `probes` adds.
pub(crate) fn library_with(native_twice: bool, probes: Option<Probes>) -> Library {
    let mut registry = FunctionRegistry::new();
    let mut ids = BTreeMap::new();
    let machine = register_machine_functions(&mut registry).unwrap();
    ids.insert("deref", machine.deref);
    ids.insert("tasks.unfinished", machine.tasks_unfinished);
    let natives: [(&'static str, &str, Arc<dyn NativeFunction>); 9] = [
        (
            "work.error",
            "() -> Any\nkernel 1\ncharge 1",
            Arc::new(LargeError),
        ),
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
            "work.repeat",
            "(length: Any) -> Any\nkernel 1\ncharge 1",
            Arc::new(Repeat),
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
        "function pair.twice(x: Any) -> Any\nkernel 1\ncharge sum(5, size(x))\nuse num.add = @{}\nnative\n\
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
    // Charged by the size of what it maps, when it returns.
    let map = parse_definition(&format!(
        "function each.map(f: Fn(x: Any) -> Any, xs: Any) -> Any\nkernel 1\ncharge size(xs)\n\
         use list.len = @{}\n\
         body {{ let out = [] for x in xs {{ let y = apply f(x) set out[list.len(out)] = y }} \
         return out }}\n",
        ids["list.len"]
    ))
    .unwrap();
    ids.insert("each.map", registry.register(map, None).unwrap());
    if let Some(probes) = probes {
        for (name, id) in probes(&mut registry, &ids) {
            ids.insert(name, id);
        }
    }
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
    pub(crate) start: CellStart,
    pub(crate) slice: u64,
    pub(crate) probes: Option<Probes>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            bounds: ROOMY,
            layout: Layout::default(),
            numbers: "by_spelling",
            native_twice: true,
            start: CellStart {
                target: Target::Main,
                args: Vec::new(),
                session: None,
                owner: PageId([9; 32]),
                modes: crate::EffectModes::recorded(),
                host: Vec::new(),
            },
            slice: u64::MAX,
            probes: None,
        }
    }
}

pub(crate) struct Embedder {
    pub(crate) machine: KernelMachine,
    pub(crate) world: World,
    pub(crate) library: Library,
    document: Arc<Document>,
    /// Every request the machine handed out, in order.
    pub(crate) requests: Vec<Request>,
    /// The requests handed out and not yet answered.
    pub(crate) pending: Vec<Request>,
    pub(crate) withdrawn: Vec<WaitId>,
    slice: u64,
    bounds: Bounds,
    probes: Option<Probes>,
    pub(crate) source: Source,
}

pub(crate) fn wait_of(request: &Request) -> WaitId {
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
pub(crate) fn answer(request: &Request) -> Outcome {
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
        Self::try_with(text, setup).unwrap_or_else(|error| panic!("{error}\n{text}"))
    }

    /// [`Self::with`], or why the machine refused to start: a bound it
    /// cannot start under.
    pub(crate) fn try_with(text: &str, setup: Setup) -> Result<Self, crate::StartError> {
        let library = library_with(setup.native_twice, setup.probes);
        let text = format!("numbers {}\n{}{text}", setup.numbers, library.header);
        let document = parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        let document = Arc::new(document);
        let program = Program {
            release: PageId([8; 32]),
            document: Arc::clone(&document),
            library: PreparedLibrary::new(Arc::clone(&library.registry)),
        };
        let source = Source::default();
        let machine = crate::KernelMachine::start_with_census(
            program,
            setup.bounds,
            setup.start,
            setup.layout,
            Box::new(source.clone()),
            census(setup.bounds.memory),
        )?;
        Ok(Self {
            machine,
            world: World::default(),
            library,
            document,
            requests: Vec::new(),
            pending: Vec::new(),
            withdrawn: Vec::new(),
            slice: setup.slice,
            bounds: setup.bounds,
            probes: setup.probes,
            source,
        })
    }

    /// The run's document with a registry built afresh, in which
    /// `pair.twice` runs natively or not.
    pub(crate) fn program(&self, native_twice: bool) -> (Program, Library) {
        let library = library_with(native_twice, self.probes);
        let program = Program {
            release: PageId([8; 32]),
            document: Arc::clone(&self.document),
            library: PreparedLibrary::new(Arc::clone(&library.registry)),
        };
        (program, library)
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

    /// Answers the oldest pending request.
    pub(crate) fn deliver_next(&mut self) {
        let next = label(self.pending.first().expect("a park with nothing pending"));
        self.deliver(&next);
    }

    /// Seals the run whole and opens it cold in another machine, built
    /// in `layout` against a program compiled afresh, in which `pair.twice`
    /// runs natively or not. The root it opened.
    pub(crate) fn reopen(&mut self, layout: Layout, native_twice: bool) -> CheckpointRoot {
        let (root, certificate) = self.checkpoint();
        self.open_at(&root, certificate, layout, native_twice);
        root
    }

    /// [`Self::reopen`], or why the seal or the open refused: a bound a
    /// law runs under can refuse at either.
    pub(crate) fn try_reopen(&mut self, layout: Layout, native_twice: bool) -> Result<(), String> {
        let (root, certificate) = self
            .try_checkpoint()
            .map_err(|error| format!("{error:?}"))?;
        let (program, library) = self.program(native_twice);
        let census = lash_kernel_state::kernel_internals::census_impl::Census::restore(
            certificate,
            LogicalBytes(self.bounds.memory),
        )
        .map_err(|error| format!("{error:?}"))?;
        self.machine = crate::KernelMachine::open_with_census(
            program,
            self.bounds,
            &root,
            Vec::new(),
            Box::new(self.source.clone()),
            Box::new(census),
            layout,
        )
        .map_err(|error| format!("{error:?}"))?;
        self.library = library;
        Ok(())
    }

    /// Seals the run whole, as a node commits it, leaving it running: the
    /// root and the accounting another node opens it with.
    pub(crate) fn checkpoint(&mut self) -> (CheckpointRoot, AccountingCertificate) {
        self.try_checkpoint().unwrap()
    }

    /// [`Self::checkpoint`], or why the machine refused to seal.
    pub(crate) fn try_checkpoint(
        &mut self,
    ) -> Result<(CheckpointRoot, AccountingCertificate), crate::SealError> {
        let sealed = self
            .machine
            .seal(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            })?
            .unwrap();
        let certificate = self.machine.accounting();
        let root: CheckpointRoot = lash_kernel_doc::graph_tokens::decode_body(
            &lash_kernel_doc::graph_tokens::encode_body(&sealed.root),
        )
        .unwrap();
        Ok((root, certificate))
    }

    /// Opens `root`, a checkpoint of this run, cold in another machine.
    pub(crate) fn open_at(
        &mut self,
        root: &CheckpointRoot,
        certificate: AccountingCertificate,
        layout: Layout,
        native_twice: bool,
    ) {
        let (program, library) = self.program(native_twice);
        self.machine = crate::KernelMachine::open_with_census(
            program,
            self.bounds,
            root,
            Vec::new(),
            Box::new(self.source.clone()),
            Box::new(
                lash_kernel_state::kernel_internals::census_impl::Census::restore(
                    certificate,
                    LogicalBytes(self.bounds.memory),
                )
                .unwrap(),
            ),
            layout,
        )
        .unwrap();
        self.library = library;
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

use lash_kernel_state::kernel_internals::census_impl::Census;
#[derive(Clone, Default)]
pub(crate) struct Source(pub(crate) Rc<RefCell<BTreeMap<PageId, PageAnswer>>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.0
                    .borrow()
                    .get(&page.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
pub(crate) fn census(bound: u64) -> Box<dyn AllocationCensus> {
    Box::new(
        Census::restore(
            AccountingCertificate {
                metric: AccountingMetric::LocalCostV1,
                upper: LogicalBytes(0),
                entities: 0,
                refinements: 0,
            },
            LogicalBytes(bound),
        )
        .unwrap(),
    )
}
pub(crate) fn start_machine(
    program: Program,
    bounds: Bounds,
    start: CellStart,
) -> Result<KernelMachine, crate::StartError> {
    crate::KernelMachine::start_with_census(
        program,
        bounds,
        start,
        crate::compile::Layout::default(),
        Box::new(Source::default()),
        census(bounds.memory),
    )
}
