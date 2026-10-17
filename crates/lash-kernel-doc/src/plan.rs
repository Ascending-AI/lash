//! Generated execution plans for library bodies (FIG-6678).
//!
//! A helper's kernel body stays its definition and the one source of its
//! meaning. A build compiles a closed body (one that reaches no guest code,
//! task action or host read) to Rust ahead of time: a [`HelperPlan`]. The
//! plan performs the body's forms in the body's order through the machine's
//! own primitives ([`PlanContext`]), so values, raises, halts, call depth
//! and memory are the body's. It never reimplements arithmetic, text or
//! heap rules: every library call goes through the machine's call path, and
//! every construction, read and write through the form the body evaluates.
//!
//! A plan runs where a native would answer and is charged as every library
//! call is, by its formula at completion (`K-CHG-003`). A plan answers every
//! call it is offered: it never declines. The body remains what runs under
//! layouts that force the fallback, and what a frame saved inside the body
//! resumes in; saved state names the body's coordinates only.
//!
//! Registers are the plan's own operand file, numbered from 0, laid out as
//! a body's operands are: operands evaluate left to right into successive
//! registers, a child reusing its result register for its
//! own operands. Slots are the body's named variables, and what each loop
//! walks.

use crate::heap::Value;
use crate::name::FunctionId;

/// A body compiled to Rust: the definition it was generated from, by
/// identity, and the code. Generated code builds these as statics; an
/// embedder installs them beside the definitions they were generated from
/// ([`crate::FunctionRegistry::install_plans`]).
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
    /// The code: it reads its parameters through the context and returns
    /// the body's result.
    pub run: fn(&mut dyn PlanContext) -> Result<Value, PlanExit>,
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

/// The machine as a plan performs a body's forms through it. Each method
/// is the form of the same name in the body, with the body's meaning:
/// errors it raises are [`PlanExit::Raise`], bounds and faults
/// [`PlanExit::Halt`].
pub trait PlanContext {
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
    /// into `args`.
    fn call(&mut self, callee: u32, args: u32, count: u32) -> Result<(), PlanExit>;
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

    /// Charges a metered loop's declared units for one test of its
    /// continuation, as the body's loop is charged at each test, even
    /// where the plan's own forms are not (`K-CHG-007`).
    fn meter(&mut self, units: u64) -> Result<(), PlanExit>;

    /// The value in `src`, for `return` or `throw`.
    fn take(&mut self, src: u32) -> Value;
}
