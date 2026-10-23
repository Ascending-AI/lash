//! A minimal embedder, in memory: it loads a document from kernel text,
//! runs it, answers its effects from a table and delivers the outcomes in
//! the order a law chooses. It uses the crate's public interface only.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{
    Datum, Document, ErrorDatum, FunctionRegistry, Integer, NativeCall, NativeError,
    NativeFunction, NewObject, NumberToken, PreparedNative, Value, parse_definition,
    parse_document,
};

use lash_kernel_state::paged::*;
use lash_vocabulary::names::FunctionId;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::{
    Bounds, CellStart, Delivered, End, Host, KernelMachine, Layout, Machine, Outcome,
    PreparedLibrary, Program, Request, RunError, Step, Target, WaitId, register_machine_functions,
};

/// Bounds no law reaches unless it sets one.
pub(crate) const ROOMY: Bounds = Bounds {
    memory_meter: crate::MemoryMeter::On,
    charge: crate::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

fn type_error(message: &'static str) -> NativeError {
    NativeError::refused("type_error", message)
}

/// Arena-independent law natives offer a prepared entry with the same
/// answer.
macro_rules! prepared {
    ($native:ident, |$args:ident| $body:expr) => {
        impl $native {
            fn answer($args: &[Value]) -> Result<Value, NativeError> {
                $body
            }
        }
        impl NativeFunction for $native {
            fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
                Self::answer(call.args.values())
            }
            fn prepare(&self) -> Option<PreparedNative> {
                Some(PreparedNative {
                    pair: None,
                    charged_entry: None,
                    entry: |args| Self::answer(args).ok(),
                    operation: stringify!($native),
                })
            }
        }
    };
}

struct Add;
prepared!(Add, |args| match args {
    [Value::Int(a), Value::Int(b)] => a
        .checked_add(*b)
        .map(Value::Int)
        .ok_or_else(|| type_error("num.add passes a word")),
    _ => Err(type_error("num.add takes two integers")),
});

struct Less;
prepared!(Less, |args| match args {
    [Value::Int(a), Value::Int(b)] => Ok(Value::Bool(a < b)),
    _ => Err(type_error("num.lt takes two integers")),
});

struct Concat;
impl NativeFunction for Concat {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let (Some(a), Some(b)) = (
            call.args
                .get(0)
                .map(|a| call.args.text(a))
                .transpose()?
                .flatten(),
            call.args
                .get(1)
                .map(|b| call.args.text(b))
                .transpose()?
                .flatten(),
        ) else {
            return Err(type_error("text.concat takes two texts"));
        };
        let text = format!("{}{}", a.whole()?, b.whole()?);
        Ok(call.new.text(&text))
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            charged_entry: None,
            entry: |args| match args {
                [Value::Text(a), Value::Text(b)] => {
                    ShortText::new(&format!("{}{}", a.as_str(), b.as_str())).map(Value::Text)
                }
                _ => None,
            },
            operation: "Concat",
        })
    }
}

struct Len;
impl NativeFunction for Len {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let Some(value) = call.args.get(0) else {
            return Err(type_error("list.len takes a collection"));
        };
        if let Some(sequence) = call.args.sequence(value)? {
            return Ok(Value::Int(sequence.len() as i64));
        }
        match call.args.table(value)? {
            Some(table) => Ok(Value::Int(table.len() as i64)),
            None => Err(type_error("list.len takes a collection")),
        }
    }
}

/// Spends one unit of its guard per step.
struct Spin;
impl NativeFunction for Spin {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [Value::Int(steps)] = call.args.values() else {
            return Err(type_error("work.spin takes an integer"));
        };
        let steps = u64::try_from(*steps).unwrap_or(0);
        for _ in 0..steps {
            call.counter.spend(1)?;
        }
        Ok(call.args.values()[0])
    }
}

/// Allocates a list of that many nulls.
struct Fill;
impl NativeFunction for Fill {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [Value::Int(length)] = call.args.values() else {
            return Err(type_error("work.fill takes an integer"));
        };
        let length = usize::try_from(*length).unwrap_or(0);
        Ok(call
            .new
            .allocate(NewObject::List(vec![Value::Null; length])))
    }
}

thread_local! {
    /// How many texts `work.repeat` has built on this law's thread.
    pub(crate) static REPEAT_BUILDS: Cell<usize> = const { Cell::new(0) };
}

/// Reserves a text of that many bytes, then builds it.
struct Repeat;
impl NativeFunction for Repeat {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [Value::Int(length)] = call.args.values() else {
            return Err(type_error("work.repeat takes an integer"));
        };
        let length = u64::try_from(*length).unwrap_or(0);
        call.new.reserve(0, length)?;
        REPEAT_BUILDS.set(REPEAT_BUILDS.get() + 1);
        Ok(call.new.text(&"x".repeat(length as usize)))
    }
}

struct Ref;
prepared!(Ref, |args| match args.first() {
    Some(
        Value::List(object)
        | Value::Map(object)
        | Value::Set(object)
        | Value::Record(object)
        | Value::Closure(object),
    ) => Ok(Value::ObjectRef(*object)),
    _ => Err(type_error("ident.ref takes an object")),
});

struct Twice;
impl NativeFunction for Twice {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let a = call.args.values()[0];
        Add::answer(&[a, a])
    }
}

/// The handful of library functions the laws call, and the header that
/// names them in a document.
struct LargeError;
impl NativeFunction for LargeError {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let kind = call.new.text(&"k".repeat(1 << 20));
        let message = call.new.text(&"m".repeat(1 << 20));
        Err(NativeError::Raised(call.new.allocate(NewObject::Error {
            kind,
            message,
            data: Value::Null,
        })))
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
    let header = String::from(
        "kernel 1\nimport 0 operation host.echo(x?: Any) -> Any\nimport 1 operation host.boom(x?: Any) -> Any\nimport 2 operation host.num(x: Text) -> Any\n",
    );
    Library {
        registry: Arc::new(registry),
        ids,
        header,
    }
}

/// The world outside the run: read-bound operations and printed values.
#[derive(Default)]
pub(crate) struct World {
    pub(crate) printed: Vec<Datum>,
    /// Each read-bound `perform`, with its arguments and its origin.
    pub(crate) performed: Vec<(lash_vocabulary::OperationUse, Vec<Datum>, Site)>,
    pub(crate) cancel: bool,
}

impl Host for World {
    /// Answers a read-bound `perform` from the effects' table.
    fn perform(
        &mut self,
        effect: &lash_vocabulary::OperationUse,
        args: &[Datum],
        _site: &lash_vocabulary::sites::PositionLabel,
        origin: &Site,
    ) -> Result<Result<Datum, ErrorDatum>, crate::HostFault> {
        self.performed
            .push((*effect, args.to_vec(), origin.clone()));
        Ok(match answer_effect(operation_name(effect.slot), args) {
            Outcome::Completed(datum) => Ok(datum),
            Outcome::Failed(error) => Err(error),
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
    pub(crate) reads: bool,
    pub(crate) cleanup: bool,
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
                modes: crate::EffectModes::new(),
                host: Vec::new(),
            },
            slice: u64::MAX,
            probes: None,
            reads: false,
            cleanup: false,
        }
    }
}

pub(crate) struct Embedder {
    pub(crate) machine: KernelMachine,
    pub(crate) world: World,
    pub(crate) library: Library,
    pub(crate) document: Arc<Document>,
    /// Every request the machine handed out, in order.
    pub(crate) requests: Vec<Request>,
    /// The requests handed out and not yet answered.
    pub(crate) pending: Vec<Request>,
    pub(crate) withdrawn: Vec<WaitId>,
    slice: u64,
    pub(crate) bounds: Bounds,
    probes: Option<Probes>,
    pub(crate) source: Source,
}

pub(crate) fn wait_of(request: &Request) -> WaitId {
    match request {
        Request::Effect(effect) => effect.wait,
    }
}

/// What a law calls a request by: an effect's first argument when it is a
/// text, else the operation's name.
pub(crate) fn label(request: &Request) -> String {
    match request {
        Request::Effect(effect) => match effect.args.first() {
            Some(Datum::Text(text)) => text.clone(),
            _ => operation_name(effect.use_.slot).into(),
        },
    }
}

/// The table effects are answered from.
pub(crate) fn answer(request: &Request) -> Outcome {
    let Request::Effect(effect) = request;
    answer_effect(operation_name(effect.use_.slot), &effect.args)
}

/// What `effect` answers `args` with: `echo` and the `dataframe`
/// fixture's `dataframe.run` their first argument, `num` its first text as
/// a number, anything else an error named by the first text.
fn answer_effect(effect: &str, args: &[Datum]) -> Outcome {
    let first = args.first().cloned().unwrap_or(Datum::Null);
    match effect {
        "echo" | "dataframe.run" => Outcome::Completed(first),
        "num" => match first {
            Datum::Text(text) => Outcome::Completed(Datum::Number(NumberToken::new(text).unwrap())),
            other => Outcome::Completed(other),
        },
        _ => Outcome::Failed(ErrorDatum {
            kind: "boom".to_string(),
            message: match &first {
                Datum::Text(text) => text.clone(),
                _ => effect.to_string(),
            },
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
        Self::try_over(text, setup, None)
    }

    /// [`Self::try_with`] over `prepared` when given: a library prepared
    /// from the registry the setup builds, which earlier runs may have used.
    pub(crate) fn try_over(
        text: &str,
        setup: Setup,
        prepared: Option<PreparedLibrary>,
    ) -> Result<Self, crate::StartError> {
        let library = library_with(setup.native_twice, setup.probes);
        let text = format!("numbers {}\n{}{text}", setup.numbers, library.header);
        let mut document = document(&text, &library.registry);
        if setup.cleanup {
            let lash_kernel_doc::Export::Function(export) = document
                .exports
                .get_mut(&lash_vocabulary::Name::new("run"))
                .unwrap()
            else {
                panic!("cleanup fixture exports run")
            };
            export.convention = lash_kernel_doc::StartConvention::PyDirect {
                cleanup: lash_kernel_doc::FunctionTarget::Local(library.ids["body.clean"]),
            };
        }
        let document = Arc::new(document);
        let program = crate::laws::embedder::program(
            Arc::clone(&document),
            prepared.unwrap_or_else(|| PreparedLibrary::new(Arc::clone(&library.registry))),
        );
        let source = Source::default();
        let mut start = setup.start;
        if let Target::Entry(name) = &start.target
            && let Some(lash_kernel_doc::Export::Function(export)) =
                program.document.exports.get(name)
        {
            let definition = &program.document.functions[&export.function];
            if definition
                .signature
                .params
                .first()
                .is_some_and(|p| p.name.as_str() == "self")
            {
                start.args.insert(
                    0,
                    Datum::Function(lash_vocabulary::CallableId {
                        link: program.link,
                        function: export.function,
                    }),
                );
            }
        }
        if setup.reads {
            start.modes = program
                .document
                .imports
                .keys()
                .map(|slot| {
                    (
                        lash_vocabulary::OperationUse {
                            link: program.link,
                            slot: *slot,
                        },
                        lash_vocabulary::package::OperationMode::Read,
                    )
                })
                .collect();
        }
        let machine = crate::KernelMachine::start_with_census(
            program,
            setup.bounds,
            start,
            setup.layout,
            Box::new(source.clone()),
            census(),
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
        let program = crate::laws::embedder::program(
            Arc::clone(&self.document),
            PreparedLibrary::new(Arc::clone(&library.registry)),
        );
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
        let census = lash_kernel_state::kernel_internals::census_impl::Census::restore(certificate)
            .map_err(|error| format!("{error:?}"))?;
        self.machine = crate::KernelMachine::open_with_census(
            program,
            self.bounds,
            &root,
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
        let root: CheckpointRoot = lash_vocabulary::tokens::decode_body(
            &lash_vocabulary::tokens::encode_body(&sealed.root),
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
            Box::new(self.source.clone()),
            Box::new(
                lash_kernel_state::kernel_internals::census_impl::Census::restore(certificate)
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
        End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        }) => error.kind,
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
    Site::new(function_unit(name), path)
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
pub(crate) fn census() -> Box<dyn AllocationCensus> {
    Box::new(Census::restore(AccountingCertificate::FRESH).unwrap())
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
        census(),
    )
}

fn operation_name(slot: lash_vocabulary::ImportSlot) -> &'static str {
    match slot.0 {
        0 => "echo",
        1 => "boom",
        2 => "num",
        3 => "dataframe.run",
        _ => panic!("unknown law operation {slot:?}"),
    }
}
/// Prepare a final Document and its Operation Uses as one linked instance.
pub(crate) fn program(document: Arc<Document>, library: PreparedLibrary) -> Program {
    use lash_vocabulary::{
        ContractId, ImportKind, OperationRequirement,
        package::{Fill, Link},
    };
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let ImportKind::Operation { signature } = &import.kind else {
                panic!("law fixture needs an operation fill")
            };
            (
                *slot,
                Fill::Operation(OperationRequirement::new(
                    signature.clone(),
                    ContractId::new("1"),
                )),
            )
        })
        .collect();
    let link = Link {
        document: document.identity(),
        fills,
    };
    Program::prepare(
        link.identity(),
        BTreeMap::from([(document.identity(), document)]),
        &BTreeMap::from([(link.identity(), link)]),
        PageId([8; 32]),
        library,
        lash_kernel_doc::Selection::Fresh,
    )
    .unwrap()
}

/// Build law Documents from final definition and main forms. Library definitions
/// are local code in this Document; their native implementations stay in the registry.
pub(crate) fn document(text: &str, registry: &FunctionRegistry) -> Document {
    use lash_kernel_doc::{Export, FunctionExport, StartConvention};

    let mut definitions: BTreeMap<_, _> = registry
        .iter()
        .map(|(id, registered)| (*id, (**registered.definition()).clone()))
        .collect();
    let mut pending = Vec::new();
    let mut main = String::new();
    for line in text.lines() {
        if let Some(source) = line.trim().strip_prefix("definition ") {
            pending.push(serde_json::from_str::<String>(source).unwrap());
        } else {
            main.push_str(line);
            main.push('\n');
        }
    }
    let imports = "import 0 operation host.echo(x?: Any) -> Any\nimport 1 operation host.boom(x?: Any) -> Any\nimport 2 operation host.num(x: Text) -> Any\n";
    let mut authored = Vec::new();
    while !pending.is_empty() {
        let before = pending.len();
        let uses: String = definitions
            .iter()
            .map(|(id, definition)| format!("use {} = @{id}\n", definition.name))
            .collect();
        pending.retain(|source| {
            match lash_kernel_doc::parse_definition_with_locals(&format!(
                "{source}\n{imports}{uses}"
            )) {
                Ok((definition, locals)) => {
                    authored.push(definition.identity());
                    definitions.extend(locals.into_iter().map(|local| (local.identity(), local)));
                    definitions.insert(definition.identity(), definition);
                    false
                }
                Err(_) => true,
            }
        });
        assert!(
            pending.len() < before,
            "law definitions do not parse: {pending:?}\n{uses}"
        );
    }
    let mut header = String::new();
    for definition in definitions.values() {
        header.push_str(&format!(
            "definition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(definition)).unwrap()
        ));
        if definition.has_native() {
            header.push_str(&format!("native @{}\n", definition.identity()));
        }
    }
    let mut document = parse_document(&format!("{header}{main}"))
        .unwrap_or_else(|error| panic!("{error}\n{header}{main}"));
    for id in authored {
        let definition = &document.functions[&id];
        document.exports.insert(
            definition.name.as_str().into(),
            Export::Function(FunctionExport {
                function: id,
                signature: definition.signature.clone(),
                convention: StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
    }
    LAW_FUNCTIONS.with_borrow_mut(|held| {
        *held = document
            .functions
            .iter()
            .map(|(id, f)| (f.name.to_string(), *id))
            .collect()
    });
    document
}
thread_local! {
    static LAW_FUNCTIONS: RefCell<BTreeMap<String, FunctionId>> = const { RefCell::new(BTreeMap::new()) };
}
pub(crate) fn function_unit(name: &str) -> Unit {
    LAW_FUNCTIONS.with_borrow(|held| Unit::Library(held[name]))
}

/// Quote interpolated kernel fragments inside a final definition's text literal.
pub(crate) fn fragment(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap();
    quoted[1..quoted.len() - 1].to_owned()
}
