//! Generated execution plans for library bodies (FIG-6678, FIG-6709).
//!
//! A helper's kernel body stays its definition and the one source of its
//! meaning. A build compiles a body to Rust ahead of time: a
//! [`HelperPlan`]. The
//! plan performs the body's forms in the body's order through the machine's
//! own primitives ([`PlanContext`]), so values, raises, halts, call depth
//! and memory are the body's. It never reimplements arithmetic, text or
//! heap rules: every library call goes through the machine's call path, and
//! every construction, read and write through the form the body evaluates.
//!
//! A plan runs where a native would answer. Formula-metered definitions pay
//! at completion (`K-CHG-003`); statement-metered definitions retain their
//! ordered form and callee charges. A plan answers every
//! call it is offered: it never declines. The body remains what runs under
//! layouts that force the fallback.
//!
//! A body that can reach guest code or the host compiles to a resumable
//! segment: at each callback site ([`PlanSite`]) the plan may yield
//! ([`PlanExit::Yield`]). The machine then holds the body's frame standing
//! at that site, exactly as the interpreted body would stand there, and
//! runs the callee as ordinary code, or waits in the site's `perform` as
//! the body's frame waits. When the callee returns or the outcome is
//! delivered, the machine runs the frame on in the body or re-enters the
//! plan after that site (`run`'s entry), as its layout chooses: the saved
//! state is the body's coordinate, never a Rust stack, so a frame parked by
//! one restores into the other.
//!
//! Registers are the plan's own operand file, numbered from 0, laid out as
//! a body's operands are: operands evaluate left to right into successive
//! registers, a child reusing its result register for its
//! own operands. Slots are the body's named variables, and what each loop
//! walks.

use crate::Value;
use lash_kernel_value::names::FunctionId;

/// A body compiled to Rust: the definition it was generated from, by
/// identity, and the code. Generated code builds these as constants with
/// promoted immutable references; an embedder installs them beside the
/// definitions they were generated from
/// ([`crate::FunctionRegistry::install_plans`]). The definition's function
/// identity identifies a plan; its address has no identity contract.
#[derive(Debug)]
pub struct HelperPlan {
    /// The name the definition carries, for diagnostics.
    pub name: &'static str,
    /// The identity of the definition whose body this plan performs.
    pub function: FunctionId,
    /// Every library function the plan calls, by identity: a plan's call
    /// names one by its index here.
    pub callees: &'static [FunctionId],
    /// The literals the body evaluates, by index.
    pub literals: &'static [PlanLiteral],
    /// The field names the body reads, writes or constructs, by index.
    pub fields: &'static [&'static str],
    /// Each record construction's field names, as indexes into `fields`.
    pub records: &'static [&'static [u32]],
    /// How many operand registers the plan uses.
    pub registers: u32,
    /// The variables the plan holds: the body's named ones by semantic
    /// declaration, and private holdings such as what each loop walks.
    pub slots: &'static [PlanLocal],
    /// Canonical paths of the closure expressions this plan evaluates.
    pub closures: &'static [&'static [u32]],
    /// The callback sites a segment yields at, by index; empty for a body
    /// that reaches neither guest code nor the host.
    pub sites: &'static [PlanSite],
    /// The code: from the call's start (entry 0), or after the callee of
    /// site `entry - 1` returned or its outcome was delivered, with the
    /// frame's state restored into the plan's variables and registers. It
    /// reads its parameters through the context and returns the body's
    /// result.
    pub run: fn(&mut dyn PlanContext, u32) -> Result<Value, PlanExit>,
    /// The body's build-staged immediate paths, when any reduce to
    /// certified native entries. The helper's own formula charges them.
    pub template: Option<HelperTemplate>,
}

/// A helper's paths over immediates: its arguments and the template's
/// resolved prepared natives, answering where a path applies.
pub type ImmediateEntry = fn(&[Value], &[crate::PreparedNative]) -> Option<Value>;

/// A scalar path that retains its metering while reducing its forms.
pub type MeteredEntry =
    fn(&[Value], &[crate::PreparedNative], &mut dyn TemplateMeter) -> Option<Value>;

/// Collects the successful scalar path's ordered charges. A failed alternative
/// rewinds its speculative events before another path is tried.
pub trait TemplateMeter {
    fn mark(&self) -> usize;
    fn rewind(&mut self, mark: usize);
    fn form(&mut self, function: FunctionId, form: FormCharge);
    fn bill(&mut self, function: FunctionId, args: &[Value], result: Value);
}

/// Value-only use of a generated template (its metered entry is used to run it).
pub struct Unmetered;
impl TemplateMeter for Unmetered {
    fn mark(&self) -> usize {
        0
    }
    fn rewind(&mut self, _: usize) {}
    fn form(&mut self, _: FunctionId, _: FormCharge) {}
    fn bill(&mut self, _: FunctionId, _: &[Value], _: Value) {}
}

/// A helper's typed paths specialized from its body at build time.
/// Callees are resolved once when the library is prepared; a missing or
/// guarded native leaves the helper on its ordinary plan or body.
#[derive(Debug)]
pub struct HelperTemplate {
    /// The paths over immediates alone, when any reduce to certified
    /// native entries.
    pub entry: Option<ImmediateEntry>,
    /// Immediate paths retaining the bodies' ordered statement charges.
    pub metered: Option<MeteredEntry>,
    pub callees: &'static [FunctionId],
    /// Conservative extra call-depth headroom, including the helper.
    /// Near the bound the body preserves the exact refusal and its name.
    pub depth: u32,
    /// The paths that read, build or write arena objects, when any do.
    pub heap: Option<HeapTemplate>,
    /// The scalar paths that forward two operands of one kind to one of
    /// [`Self::callees`] unchanged.
    pub forwards: Forwards,
}

/// Which resolved native a template's paths over two floats, two word
/// integers or two texts answer as exactly: the path is that native's call on the
/// same two arguments, and nothing else.
#[derive(Clone, Copy, Debug, Default)]
pub struct Forwards {
    pub floats: Option<u8>,
    pub ints: Option<u8>,
    /// All four combinations of inline and long texts forward these same
    /// operands to this native, with no other calls, guards or forms.
    pub texts: Option<u8>,
}

/// A helper's paths that reach the arena: each reads, builds and writes
/// through a [`TemplateHeap`] the body's own forms in the body's order,
/// and declines where the body would raise, park, reach guest code or do
/// what the template does not. A path's last fallible step precedes its
/// one write, if it writes: a path that declines has written nothing. Its
/// immediate calls use the template's resolved prepared natives
/// ([`HelperTemplate::callees`]), its collection lengths and membership
/// tests [`TemplateHeap::len`] and [`TemplateHeap::lookup`] (FIG-6815),
/// its other calls the general call of `callees`.
#[derive(Debug)]
pub struct HeapTemplate {
    pub entry: fn(&mut dyn TemplateHeap, &[Value], &[crate::PreparedNative]) -> Option<Value>,
    /// The natives its paths call through [`TemplateHeap::call`], by index.
    pub callees: &'static [FunctionId],
    /// Conservative extra call-depth headroom, including the helper.
    pub depth: u32,
    /// Whether a path writes an arena object ([`TemplateHeap::set_index`]):
    /// the write is the path's last fallible step, but what runs after
    /// the call in its caller's statement may still fail.
    pub writes: bool,
    /// A certified numeric array access, over unchanged arguments.
    pub element: Option<Element>,
    /// A source-certified record membership guard followed by the write
    /// `target[key] = value`, returning `value`, over three unchanged
    /// arguments. Other operand kinds retain the ordinary template.
    pub record_write: Option<RecordWrite>,
}

/// A helper's operation on a list and an integral nonnegative Float key
/// below [`ELEMENT_POSITIONS`]. Other operand paths keep their source body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Element {
    /// Return the indexed value, converting an empty tuple hole to Absent.
    /// The key must be strictly below the list's length.
    Read,
    /// Write the third argument and return it. The key may equal length.
    Write,
}

/// The symbolic record-write path retained by a helper template.
#[derive(Clone, Copy, Debug)]
pub struct RecordWrite {
    /// The constant key whose membership the path tests.
    pub key: &'static str,
    /// The membership predicate in [`HelperTemplate::callees`].
    pub test: u8,
    /// The boolean answer that selects the write branch.
    pub holds: bool,
    /// The accepted written-value kinds, one bit per [`crate::ValueKind`].
    pub values: u32,
}

/// The positions an element-writing template writes at lie below
/// ([`HeapTemplate::element`]): 2³² − 1.
pub const ELEMENT_POSITIONS: u64 = (1 << 32) - 1;

/// What a heap path reads, builds and writes with: the forms a helper body
/// performs on the arena, as the machine performs them for the body. Each
/// answers `None`, having changed nothing a run sees, where the body's form
/// would raise or the arena must decode first; the path then declines.
pub trait TemplateHeap: TemplateMeter {
    /// The general call of [`HeapTemplate::callees`]`[callee]` on `args`,
    /// uncharged, as a body's own call of a native or invoke of a helper is.
    fn call(&mut self, callee: usize, args: &[Value]) -> Option<Value>;
    /// The body's variable declared with `value`, or assigned it over
    /// `previous`: the memory the plan's variable grows by, held as the
    /// plan holds it once the path answers. A path binds no immediate,
    /// which grows nothing the arena holds.
    fn bind(&mut self, value: Value, previous: Option<Value>) -> Option<()>;
    /// An alternative path begins: what an earlier one declared is not its.
    fn path(&mut self);
    /// `target[index]`.
    fn index(&mut self, target: Value, index: Value) -> Option<Value>;
    /// `target.field`.
    fn field(&mut self, target: Value, field: &str) -> Option<Value>;
    /// The tuple of `items`.
    fn tuple(&mut self, items: &[Value]) -> Option<Value>;
    /// A source-certified shallow concatenation. The body reads each
    /// present list member through an ordinary charged closure costing
    /// `read_units` plus its immutable nested comparison size; holes are
    /// copied without that read. Records retain their branded-array path.
    fn concat(&mut self, receiver: Value, arguments: Value, read_units: u64) -> Option<Value> {
        let _ = (receiver, arguments, read_units);
        None
    }
    /// A certified dense snapshot, converting holes to Absent and retaining
    /// the body's charged reads and all continuation tests of its loop.
    fn dense(&mut self, source: Value, read_units: u64, loop_units: u64) -> Option<Value> {
        let _ = (source, read_units, loop_units);
        None
    }
    /// `set target[index] = value`: its growth held and what it changes
    /// marked dirty, or nothing written.
    fn set_index(&mut self, target: Value, index: Value, value: Value) -> Option<()>;
    /// `<collection>.len(target)`, as the native answers it.
    fn len(&mut self, collection: Collection, target: Value) -> Option<Value>;
    /// `<table>.contains(target, key)`, as the native answers it, and the
    /// entry `target[key]` reads where it holds, from one probe of the
    /// table (FIG-6846): `Some(None)` where the table holds no `key`. A
    /// set's member reads as `true`.
    fn lookup(&mut self, table: Table, target: Value, key: Value) -> Option<Option<Value>>;
}

/// The collection a heap path's length read names: `list.len` reads a
/// list, `map.len` a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    List,
    Tuple,
    Map,
    Set,
    Record,
}

/// The table a heap path's membership test names: `map.contains` tests a
/// map's keys, `record.contains` a record's fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Map,
    Set,
    Record,
}

/// A callback site: an action of the body where the plan may yield, and
/// what the body's frame holds standing there (`K-EVAL-008`).
#[derive(Clone, Copy, Debug)]
pub struct PlanSite {
    /// The action's site, as a path under the body's unit.
    pub path: &'static [u32],
    /// What the action calls.
    pub call: SiteCall,
    /// The register the action's value goes to.
    pub dst: u32,
    /// The operands the action's statement has evaluated when the action
    /// is reached: each by its site's path and the register holding it.
    pub roles: &'static [PlanRole],
    /// The plan variables in scope there.
    pub bound: &'static [u32],
    /// The loops the site stands in, outermost first.
    pub loops: &'static [PlanLoop],
}

/// What a callback site calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteCall {
    /// The value in the action's register, its arguments after it.
    Apply,
    /// Callee `index` of the plan, which may run its body in a frame.
    Invoke(u32),
    /// The body's `perform` at the site: the effect, its result type and
    /// its arguments are the body's action's, read from the body.
    Perform,
}

/// An operand an action's statement holds when the action is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanRole {
    pub path: &'static [u32],
    pub reg: u32,
}

/// A loop a callback site stands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanLoop {
    /// A `for`, what it walks held in this plan variable.
    For {
        walked: u32,
    },
    While,
}

/// A variable's semantic declaration, resolved to the body's own slot and
/// sharing rule at preparation. An empty name marks a private loop holding.
#[derive(Debug)]
pub struct PlanLocal {
    /// The name bound by the declaration, or empty for a private holding.
    pub name: &'static str,
    /// The declaring node's canonical path within the helper's library unit.
    pub path: &'static [u32],
}

/// A literal a plan evaluates, as the body writes it.
#[derive(Clone, Copy, Debug)]
pub enum PlanLiteral {
    Null,
    Absent,
    Bool(bool),
    /// An integer in its decimal text (`K-VAL-032`).
    Int(&'static str),
    /// A float by its bits.
    Float(u64),
    Text(&'static str),
    Bytes(&'static [u8]),
}

/// How a plan stops without a value.
#[derive(Debug)]
pub enum PlanExit {
    /// The body raises this value.
    Raise(Value),
    /// The run ends (a bound, the heap): the context holds why. Nothing in
    /// the plan catches it.
    Halt,
    /// The plan stopped at a callback site: the machine holds the body's
    /// frame standing there and runs the callee, or waits in the site's
    /// `perform`. Nothing in the plan
    /// catches it, and no plan state survives it outside the machine.
    Yield,
}

/// What a test of a condition is for: it names the error a value that is
/// not a bool raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Test {
    /// An `if` statement's or a conditional expression's condition.
    If,
    /// A `while` loop's condition.
    While,
}

/// Reads a function identity's 64 lower-case hexadecimal digits, as a
/// generated plan spells its identities. A malformed spelling does not
/// compile.
#[must_use]
pub const fn function(hex: &str) -> FunctionId {
    let hex = hex.as_bytes();
    assert!(hex.len() == 64, "an identity has 64 hexadecimal digits");
    let mut digest = [0u8; 32];
    let mut at = 0;
    while at < 32 {
        digest[at] = (digit(hex[2 * at]) << 4) | digit(hex[2 * at + 1]);
        at += 1;
    }
    FunctionId::from_bytes(digest)
}

const fn digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("an identity is lower-case hexadecimal"),
    }
}

/// A `for` loop's cursor, opened by [`PlanContext::open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor(pub u32);

/// A statement-metered body's fixed charge points, in evaluation order.
#[derive(Clone, Copy)]
pub enum FormCharge {
    Statement,
    Expression,
    Argument,
    Construction(u32),
    LoopTest,
}

/// The machine as a plan performs a body's forms through it. Each method
/// is the form of the same name in the body, with the body's meaning:
/// errors it raises are [`PlanExit::Raise`], bounds and faults
/// [`PlanExit::Halt`].
pub trait PlanContext {
    /// Charges a form of a statement-metered body before it executes.
    /// Under charge Off only statements, arguments and loop tests pace it.
    fn charge(&mut self, form: FormCharge) -> Result<(), PlanExit>;

    /// Opens the next statement of the body, ending the one open: its
    /// operands, pins and write credit are released, as the body's
    /// statement's are at its end.
    fn begin(&mut self) -> Result<(), PlanExit>;

    /// Parameter `index`'s argument into `dst`.
    fn param(&mut self, index: u32, dst: u32) -> Result<(), PlanExit>;
    /// Binds variable `slot` to parameter `index`, using the body's shared
    /// cell when captured. The original argument stays for the formula.
    fn adopt(&mut self, index: u32, slot: u32) -> Result<(), PlanExit>;
    /// Literal `literal` into `dst`.
    fn literal(&mut self, literal: u32, dst: u32) -> Result<(), PlanExit>;
    /// Variable `slot` into `dst`; an empty one raises `unbound_variable`.
    fn local(&mut self, slot: u32, dst: u32) -> Result<(), PlanExit>;
    /// Declares variable `slot` with the value in `src` (`K-FORM-004`).
    fn bind(&mut self, slot: u32, src: u32) -> Result<(), PlanExit>;
    /// Writes variable `slot` with the value in `src`.
    fn assign(&mut self, slot: u32, src: u32) -> Result<(), PlanExit>;
    /// Ends the scope of variable `slot`.
    fn unbind(&mut self, slot: u32);

    /// Creates the body's closure at `closure`, sharing its original cells.
    fn closure(&mut self, closure: u32, dst: u32) -> Result<(), PlanExit>;
    /// Holds a departing or caught value across cleanup statements in `slot`.
    fn retain(&mut self, value: Value, slot: u32) -> Result<Value, PlanExit>;
    /// The open-loop depth at a protected block's entry.
    fn scope(&self) -> u32;
    /// Leaves every loop opened inside a protected block.
    fn unwind(&mut self, scope: u32);

    /// Calls `callee` with the `count` arguments from `args`, its value
    /// into `args`. The callee answers at once.
    fn call(&mut self, callee: u32, args: u32, count: u32) -> Result<(), PlanExit>;
    /// Callback site `site`: calls `callee`, which may run its body in a
    /// frame, with the `count` arguments from `args`, its value into
    /// `args`, or yields there.
    fn invoke(&mut self, site: u32, callee: u32, args: u32, count: u32) -> Result<(), PlanExit>;
    /// Callback site `site`: applies the value in `at` to the `count`
    /// arguments after it, its value into `at`, or yields there.
    fn apply(&mut self, site: u32, at: u32, count: u32) -> Result<(), PlanExit>;
    /// Callback site `site`: the body's `perform` there, on the `count`
    /// arguments from `args`. A read-bound effect answers at once, its
    /// value into `args`; any other yields, the body's frame waiting in it.
    fn perform(&mut self, site: u32, args: u32, count: u32) -> Result<(), PlanExit>;
    /// Field `field` of the record or error in `at`, into `at`.
    fn field(&mut self, at: u32, field: u32) -> Result<(), PlanExit>;
    /// What `at` holds at the index in the register after, into `at`.
    fn index(&mut self, at: u32) -> Result<(), PlanExit>;
    fn tuple(&mut self, start: u32, len: u32) -> Result<(), PlanExit>;
    fn list(&mut self, start: u32, len: u32) -> Result<(), PlanExit>;
    fn set(&mut self, start: u32, len: u32) -> Result<(), PlanExit>;
    /// `len` registers from `start`: key, value, key, value, ...
    fn map(&mut self, start: u32, len: u32) -> Result<(), PlanExit>;
    /// Record construction `record`'s fields from the registers from `start`.
    fn record(&mut self, record: u32, start: u32) -> Result<(), PlanExit>;
    /// Keeps the value in `src` live until the statement ends.
    fn pin(&mut self, src: u32) -> Result<(), PlanExit>;
    fn set_field(&mut self, target: u32, field: u32, src: u32) -> Result<(), PlanExit>;
    /// Writes the index in the register after `target`.
    fn set_index(&mut self, target: u32, src: u32) -> Result<(), PlanExit>;
    fn remove_field(&mut self, target: u32, field: u32) -> Result<(), PlanExit>;
    /// Removes the index in the register after `target`.
    fn remove_index(&mut self, target: u32) -> Result<(), PlanExit>;

    /// Takes the bool in `cond`; any other value raises `type_error`.
    fn test(&mut self, cond: u32, test: Test) -> Result<bool, PlanExit>;
    /// Starts a `for` over the value in `iterable` (`K-ITER-001`), held
    /// in variable `slot` while the loop walks it.
    fn open(&mut self, iterable: u32, slot: u32) -> Result<Cursor, PlanExit>;
    /// The loop's next element into `dst`, or `false` when it is done
    /// (`K-ITER-002`, `K-ITER-003`).
    fn next(&mut self, cursor: Cursor, dst: u32) -> Result<bool, PlanExit>;
    /// Leaves the loop of `cursor` and every loop opened after it.
    fn close(&mut self, cursor: Cursor);
    /// Starts a `while` a callback site stands in: its passes are counted,
    /// as the body's frame counts them.
    fn enter_while(&mut self);
    /// The innermost counted `while` starts another pass.
    fn pass_while(&mut self);
    /// Leaves every counted `while` but the outermost `depth`.
    fn leave_while(&mut self, depth: u32);

    /// Charges a metered loop's declared units for one test of its
    /// continuation, as the body's loop is charged at each test, even
    /// where the plan's own forms are not (`K-CHG-007`).
    fn meter(&mut self, units: u64) -> Result<(), PlanExit>;

    /// A resume that reached the body's end without finding the site of
    /// `entry`: a malformed plan, which ends the run.
    fn lost(&mut self, entry: u32) -> PlanExit;

    /// The value in `src`, for `return` or `throw`.
    fn take(&mut self, src: u32) -> Value;
}
