//! Hand-lowered Kir functions for building and testing tiers before
//! lowering exists.
//!
//! Each [`Fixture`] is one kernel function, its source, its Kir and its
//! baseline maps, and passes [`crate::verify_function`]. Together they
//! reach every terminator, a loop of each kind, an action with a
//! continuation, exception edges and every `finally` departure kind a
//! straight body can take. Charge anchors follow the interpreter's event
//! order as the design states it; lowering, once it exists, is the
//! authority on exact placement, and these fixtures are not a lowering
//! oracle. Library callees are named by placeholder identities
//! ([`num_lt`], [`num_add`]), which no registry holds.

use lash_kernel_doc::{FunctionId, Integer, Name, Site, Unit, Value};

use crate::boundary::{Boundary, BoundaryKind, ControlRecipe, CtlStatic, TryPhaseStatic};
use crate::kir::{
    ActionIx, ActionKind, BoundaryIx, CalleeRef, CodeIx, ConstIx, Departure, FieldIx, KAction,
    KBlock, KBlockIx, KLoop, KTry, KirFunction, LibIx, LocalRef, LoopIx, LoopKind, Op, OpOrigin,
    OpRange, Origin, Reg, RegRange, Scope, ScopeIx, SlotInfo, StmtIx, TryIx, VarRef,
};
use crate::maps::{FrameMap, baseline_maps};
use crate::sem::ChargeEvent;

/// The placeholder identity of `num.lt` in the fixtures.
pub fn num_lt() -> FunctionId {
    FunctionId::from_bytes([0x11; 32])
}

/// The placeholder identity of `num.add` in the fixtures.
pub fn num_add() -> FunctionId {
    FunctionId::from_bytes([0x12; 32])
}

/// One hand-lowered function.
#[derive(Clone, Debug, PartialEq)]
pub struct Fixture {
    pub name: &'static str,
    /// The kernel source the Kir lowers.
    pub source: &'static str,
    pub function: KirFunction,
    /// One baseline map per Boundary.
    pub maps: Vec<FrameMap>,
}

/// Every fixture.
pub fn all() -> Vec<Fixture> {
    vec![
        pick_field(),
        count_to(),
        sum_items(),
        call_helper(),
        guarded(),
    ]
}

/// Straight-line code: a member read and a return.
pub fn pick_field() -> Fixture {
    let mut b = Builder::new("pick", 3);
    let r = b.slot("r", &[], false);
    let x = b.slot("x", &[0], false);
    let body = b.scope(&[], &[r, x]);
    b.param(r);
    let a = b.field("a");
    let s0 = b.stmt(&[0]);
    let s1 = b.stmt(&[1]);
    let b0 = b.boundary(s0, BoundaryKind::StatementStart, &[blk(body, 0)], None);
    let b1 = b.boundary(s1, BoundaryKind::StatementStart, &[blk(body, 1)], None);

    b.block(None);
    b.op(Op::Stmt { boundary: b0 });
    b.op(charge(2, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(0),
        var: VarRef::Local(r),
    });
    b.op(Op::Field {
        dst: Reg(1),
        target: Reg(0),
        field: a,
    });
    b.op(Op::Bind {
        local: x,
        src: Reg(1),
    });
    b.op(Op::Stmt { boundary: b1 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(2),
        var: VarRef::Local(x),
    });
    b.op(Op::Return { src: Reg(2) });
    b.finish("pick_field", "fn pick(r) {\n  let x = r.a\n  return x\n}\n")
}

/// A `while` loop whose test and body call natives.
pub fn count_to() -> Fixture {
    let mut b = Builder::new("count", 8);
    let n = b.slot("n", &[], false);
    let i = b.slot("i", &[0], false);
    let body = b.scope(&[], &[n, i]);
    let inner = b.scope(&[1, 1], &[]);
    b.param(n);
    let zero = b.constant(Value::Int(Integer::from(0)));
    let one = b.constant(Value::Int(Integer::from(1)));
    let lt = b.lib(num_lt());
    let add = b.lib(num_add());
    let s0 = b.stmt(&[0]);
    let s1 = b.stmt(&[1]);
    let s2 = b.stmt(&[1, 1, 0]);
    let s3 = b.stmt(&[2]);
    let lp = b.lp(KLoop {
        stmt: s1,
        kind: LoopKind::While,
        test: KBlockIx(1),
        exit: KBlockIx(3),
    });
    let in_loop = [blk(body, 1), CtlStatic::While { lp }];
    let b0 = b.boundary(s0, BoundaryKind::StatementStart, &[blk(body, 0)], None);
    let b1 = b.boundary(s1, BoundaryKind::StatementStart, &[blk(body, 1)], None);
    let b2 = b.boundary(s1, BoundaryKind::LoopTest, &in_loop, None);
    let b3 = b.boundary(
        s2,
        BoundaryKind::StatementStart,
        &[in_loop[0], in_loop[1], blk(inner, 0)],
        None,
    );
    let b4 = b.boundary_at(
        &[1, 1],
        BoundaryKind::BlockEnd,
        &[in_loop[0], in_loop[1], blk(inner, 1)],
    );
    let b5 = b.boundary(s3, BoundaryKind::StatementStart, &[blk(body, 2)], None);

    b.block(None);
    b.op(Op::Stmt { boundary: b0 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Const {
        dst: Reg(0),
        c: zero,
    });
    b.op(Op::Bind {
        local: i,
        src: Reg(0),
    });
    b.op(Op::Stmt { boundary: b1 });
    b.op(Op::WhileInit { lp });
    b.op(Op::Jump { to: KBlockIx(1) });

    b.block(None);
    b.op(Op::Step { boundary: b2 });
    b.op(charge(1, ChargeEvent::LoopTest));
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(charge(2, ChargeEvent::AtomArg));
    b.op(Op::Read {
        dst: Reg(1),
        var: VarRef::Local(i),
    });
    b.op(Op::Read {
        dst: Reg(2),
        var: VarRef::Local(n),
    });
    b.op(Op::CallNative {
        dst: Reg(3),
        lib: lt,
        args: regs(1, 2),
    });
    b.op(Op::WhileTest {
        lp,
        cond: Reg(3),
        body: KBlockIx(2),
        done: KBlockIx(3),
    });

    b.block(None);
    b.op(Op::Enter { scope: inner });
    b.op(Op::Stmt { boundary: b3 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(charge(2, ChargeEvent::AtomArg));
    b.op(Op::Read {
        dst: Reg(4),
        var: VarRef::Local(i),
    });
    b.op(Op::Const {
        dst: Reg(5),
        c: one,
    });
    b.op(Op::CallNative {
        dst: Reg(6),
        lib: add,
        args: regs(4, 2),
    });
    b.op(Op::Write {
        var: VarRef::Local(i),
        src: Reg(6),
    });
    b.op(Op::Step { boundary: b4 });
    b.op(Op::Leave { scope: inner });
    b.op(Op::Jump { to: KBlockIx(1) });

    b.block(None);
    b.op(Op::LoopExit { lp });
    b.op(Op::Stmt { boundary: b5 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(7),
        var: VarRef::Local(i),
    });
    b.op(Op::Return { src: Reg(7) });
    b.finish(
        "count_to",
        "fn count(n) {\n  let i = 0\n  while num.lt(i, n) {\n    set i = num.add(i, 1)\n  }\n  return i\n}\n",
    )
}

/// A `for` loop over a value.
pub fn sum_items() -> Fixture {
    let mut b = Builder::new("total", 6);
    let xs = b.slot("xs", &[], false);
    let sum = b.slot("sum", &[0], false);
    let x = b.slot("x", &[1], false);
    let body = b.scope(&[], &[xs, sum]);
    let inner = b.scope(&[1, 1], &[x]);
    b.param(xs);
    let zero = b.constant(Value::Int(Integer::from(0)));
    let add = b.lib(num_add());
    let s0 = b.stmt(&[0]);
    let s1 = b.stmt(&[1]);
    let s2 = b.stmt(&[1, 1, 0]);
    let s3 = b.stmt(&[2]);
    let lp = b.lp(KLoop {
        stmt: s1,
        kind: LoopKind::For { binding: x },
        test: KBlockIx(1),
        exit: KBlockIx(3),
    });
    let in_loop = [blk(body, 1), CtlStatic::For { lp }];
    let b0 = b.boundary(s0, BoundaryKind::StatementStart, &[blk(body, 0)], None);
    let b1 = b.boundary(s1, BoundaryKind::StatementStart, &[blk(body, 1)], None);
    let b2 = b.boundary(s1, BoundaryKind::LoopTest, &in_loop, None);
    let b3 = b.boundary(
        s2,
        BoundaryKind::StatementStart,
        &[in_loop[0], in_loop[1], blk(inner, 0)],
        None,
    );
    let b4 = b.boundary_at(
        &[1, 1],
        BoundaryKind::BlockEnd,
        &[in_loop[0], in_loop[1], blk(inner, 1)],
    );
    let b5 = b.boundary(s3, BoundaryKind::StatementStart, &[blk(body, 2)], None);

    b.block(None);
    b.op(Op::Stmt { boundary: b0 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Const {
        dst: Reg(0),
        c: zero,
    });
    b.op(Op::Bind {
        local: sum,
        src: Reg(0),
    });
    b.op(Op::Stmt { boundary: b1 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(1),
        var: VarRef::Local(xs),
    });
    b.op(Op::ForInit {
        lp,
        iterable: Reg(1),
    });
    b.op(Op::Jump { to: KBlockIx(1) });

    b.block(None);
    b.op(Op::Step { boundary: b2 });
    b.op(charge(1, ChargeEvent::LoopTest));
    b.op(Op::ForNext {
        lp,
        bind: x,
        body: KBlockIx(2),
        done: KBlockIx(3),
    });

    b.block(None);
    b.op(Op::Enter { scope: inner });
    b.op(Op::Stmt { boundary: b3 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(charge(2, ChargeEvent::AtomArg));
    b.op(Op::Read {
        dst: Reg(2),
        var: VarRef::Local(sum),
    });
    b.op(Op::Read {
        dst: Reg(3),
        var: VarRef::Local(x),
    });
    b.op(Op::CallNative {
        dst: Reg(4),
        lib: add,
        args: regs(2, 2),
    });
    b.op(Op::Write {
        var: VarRef::Local(sum),
        src: Reg(4),
    });
    b.op(Op::Step { boundary: b4 });
    b.op(Op::Leave { scope: inner });
    b.op(Op::Jump { to: KBlockIx(1) });

    b.block(None);
    b.op(Op::LoopExit { lp });
    b.op(Op::Stmt { boundary: b5 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(5),
        var: VarRef::Local(sum),
    });
    b.op(Op::Return { src: Reg(5) });
    b.finish(
        "sum_items",
        "fn total(xs) {\n  let sum = 0\n  for x in xs {\n    set sum = num.add(sum, x)\n  }\n  return sum\n}\n",
    )
}

/// A call statement: the activation exits `Act` and continues at the
/// call-return label with the callee's result.
pub fn call_helper() -> Fixture {
    let mut b = Builder::new("twice", 3);
    let x = b.slot("x", &[], false);
    let y = b.slot("y", &[0], false);
    let body = b.scope(&[], &[x, y]);
    b.param(x);
    let double = b.code(Origin::Document(Unit::Function(Name::new("double"))));
    let s0 = b.stmt(&[0]);
    let s1 = b.stmt(&[1]);
    let call = b.action(
        &[0],
        ActionKind::Call {
            callee: CalleeRef::Declared(double),
            args: regs(0, 1),
        },
    );
    let b0 = b.boundary(s0, BoundaryKind::StatementStart, &[blk(body, 0)], None);
    let b1 = b.boundary(s0, BoundaryKind::CallReturn, &[blk(body, 0)], Some(s0));
    let b2 = b.boundary(s1, BoundaryKind::StatementStart, &[blk(body, 1)], None);

    b.block(None);
    b.op(Op::Stmt { boundary: b0 });
    b.op(charge(1, ChargeEvent::AtomArg));
    b.op(Op::Read {
        dst: Reg(0),
        var: VarRef::Local(x),
    });
    b.op(Op::Act {
        dst: Reg(1),
        action: call,
        boundary: b1,
        resume: KBlockIx(1),
    });

    b.block(None);
    b.op(Op::Bind {
        local: y,
        src: Reg(1),
    });
    b.op(Op::Stmt { boundary: b2 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(2),
        var: VarRef::Local(y),
    });
    b.op(Op::Return { src: Reg(2) });
    b.finish(
        "call_helper",
        "fn twice(x) {\n  let y = call double(x)\n  return y\n}\n",
    )
}

/// `try`, `catch` and `finally`: exception edges into the handler and
/// into the `finally`, and a `finally` entered normally and by a raise.
pub fn guarded() -> Fixture {
    let mut b = Builder::new("guarded", 7);
    let r = b.slot("r", &[], false);
    let e = b.slot("e", &[0, 1], false);
    let body = b.scope(&[], &[r]);
    let tried = b.scope(&[0, 0], &[]);
    let caught = b.scope(&[0, 1], &[e]);
    let last = b.scope(&[0, 2], &[]);
    b.param(r);
    let a = b.field("a");
    let done = b.constant(Value::text("done"));
    let null = b.constant(Value::Null);
    let s0 = b.stmt(&[0]);
    let s1 = b.stmt(&[0, 0, 0]);
    let s2 = b.stmt(&[0, 1, 0]);
    let s3 = b.stmt(&[0, 2, 0]);
    let s4 = b.stmt(&[1]);
    let t = b.tr(KTry {
        stmt: s0,
        catch: Some((e, KBlockIx(2))),
        finally: Some(KBlockIx(4)),
    });
    let phase = |phase| CtlStatic::Try { t, phase };
    let b0 = b.boundary(s0, BoundaryKind::StatementStart, &[blk(body, 0)], None);
    let in_body = [blk(body, 0), phase(TryPhaseStatic::Body)];
    let b1 = b.boundary(
        s1,
        BoundaryKind::StatementStart,
        &[in_body[0], in_body[1], blk(tried, 0)],
        None,
    );
    let b2 = b.boundary_at(
        &[0, 0],
        BoundaryKind::BlockEnd,
        &[in_body[0], in_body[1], blk(tried, 1)],
    );
    let in_catch = [blk(body, 0), phase(TryPhaseStatic::Catch)];
    let b3 = b.boundary(
        s2,
        BoundaryKind::StatementStart,
        &[in_catch[0], in_catch[1], blk(caught, 0)],
        None,
    );
    let b4 = b.boundary_at(
        &[0, 1],
        BoundaryKind::BlockEnd,
        &[in_catch[0], in_catch[1], blk(caught, 1)],
    );
    let in_finally = [blk(body, 0), phase(TryPhaseStatic::Finally)];
    let b5 = b.boundary(
        s3,
        BoundaryKind::StatementStart,
        &[in_finally[0], in_finally[1], blk(last, 0)],
        None,
    );
    let b6 = b.boundary_at(
        &[0, 2],
        BoundaryKind::BlockEnd,
        &[in_finally[0], in_finally[1], blk(last, 1)],
    );
    let b7 = b.boundary(s4, BoundaryKind::StatementStart, &[blk(body, 1)], None);

    b.block(None);
    b.op(Op::Stmt { boundary: b0 });
    b.op(Op::TryEnter { t });
    b.op(Op::Jump { to: KBlockIx(1) });

    b.block(Some(KBlockIx(2)));
    b.op(Op::Enter { scope: tried });
    b.op(Op::Stmt { boundary: b1 });
    b.op(charge(2, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(0),
        var: VarRef::Local(r),
    });
    b.op(Op::Field {
        dst: Reg(1),
        target: Reg(0),
        field: a,
    });
    b.op(Op::Print { src: Reg(1) });
    b.op(Op::Step { boundary: b2 });
    b.op(Op::Leave { scope: tried });
    b.op(Op::FinallyEnter {
        t,
        departure: Departure::Normal,
    });
    b.op(Op::Jump { to: KBlockIx(4) });

    b.block(Some(KBlockIx(3)));
    b.op(Op::Caught { dst: Reg(2) });
    b.op(Op::CatchEnter { t });
    b.op(Op::Enter { scope: caught });
    b.op(Op::Bind {
        local: e,
        src: Reg(2),
    });
    b.op(Op::Stmt { boundary: b3 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Read {
        dst: Reg(3),
        var: VarRef::Local(e),
    });
    b.op(Op::Print { src: Reg(3) });
    b.op(Op::Step { boundary: b4 });
    b.op(Op::Leave { scope: caught });
    b.op(Op::FinallyEnter {
        t,
        departure: Departure::Normal,
    });
    b.op(Op::Jump { to: KBlockIx(4) });

    b.block(None);
    b.op(Op::Caught { dst: Reg(4) });
    b.op(Op::FinallyEnter {
        t,
        departure: Departure::Throw(Reg(4)),
    });
    b.op(Op::Jump { to: KBlockIx(4) });

    b.block(None);
    b.op(Op::Enter { scope: last });
    b.op(Op::Stmt { boundary: b5 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Const {
        dst: Reg(5),
        c: done,
    });
    b.op(Op::Print { src: Reg(5) });
    b.op(Op::Step { boundary: b6 });
    b.op(Op::Leave { scope: last });
    b.op(Op::FinallyEnd {
        t,
        normal: KBlockIx(5),
    });

    b.block(None);
    b.op(Op::TryExit { t });
    b.op(Op::Stmt { boundary: b7 });
    b.op(charge(1, ChargeEvent::ExprNode));
    b.op(Op::Const {
        dst: Reg(6),
        c: null,
    });
    b.op(Op::Return { src: Reg(6) });
    b.finish(
        "guarded",
        "fn guarded(r) {\n  try {\n    print r.a\n  } catch e {\n    print e\n  } finally {\n    print \"done\"\n  }\n  return null\n}\n",
    )
}

const fn charge(units: u64, event: ChargeEvent) -> Op {
    Op::Charge { units, event }
}

const fn regs(start: u32, len: u32) -> RegRange {
    RegRange {
        start: Reg(start),
        len,
    }
}

const fn blk(scope: ScopeIx, next: u32) -> CtlStatic {
    CtlStatic::Block { scope, next }
}

/// Assembles a [`KirFunction`] table by table, blocks in index order.
struct Builder {
    unit: Unit,
    f: KirFunction,
    ops: Vec<Op>,
    origins: Vec<OpOrigin>,
    blocks: Vec<KBlock>,
    open: Option<(u32, Option<KBlockIx>)>,
    stmt: Site,
}

impl Builder {
    fn new(name: &str, regs: u32) -> Self {
        let unit = Unit::Function(Name::new(name));
        Self {
            f: KirFunction {
                origin: Origin::Document(unit.clone()),
                charged: true,
                params: Box::new([]),
                slots: Box::new([]),
                ops: Box::new([]),
                blocks: Box::new([]),
                boundaries: Box::new([]),
                regs,
                consts: Box::new([]),
                names: Box::new([]),
                fields: Box::new([]),
                field_lists: Box::new([]),
                codes: Box::new([]),
                libs: Box::new([]),
                stmts: Box::new([]),
                scopes: Box::new([]),
                actions: Box::new([]),
                loops: Box::new([]),
                tries: Box::new([]),
                origins: Box::new([]),
            },
            stmt: Site::new(unit.clone(), Vec::new()),
            unit,
            ops: Vec::new(),
            origins: Vec::new(),
            blocks: Vec::new(),
            open: None,
        }
    }

    fn site(&self, path: &[u32]) -> Site {
        Site::new(self.unit.clone(), path)
    }

    fn slot(&mut self, name: &str, declared: &[u32], shared: bool) -> LocalRef {
        let at = self.f.slots.len() as u32;
        let info = SlotInfo {
            name: Name::new(name),
            declared: self.site(declared),
            shared,
        };
        push(&mut self.f.slots, info);
        LocalRef {
            slot: at,
            at,
            shared,
        }
    }

    fn param(&mut self, local: LocalRef) {
        push(&mut self.f.params, local);
    }

    fn scope(&mut self, path: &[u32], declares: &[LocalRef]) -> ScopeIx {
        let scope = Scope {
            site: self.site(path),
            declares: declares.iter().map(|local| local.at).collect(),
        };
        ScopeIx(push(&mut self.f.scopes, scope))
    }

    fn field(&mut self, name: &str) -> FieldIx {
        FieldIx(push(&mut self.f.fields, name.into()))
    }

    fn constant(&mut self, value: Value) -> ConstIx {
        ConstIx(push(&mut self.f.consts, value))
    }

    fn lib(&mut self, id: FunctionId) -> LibIx {
        LibIx(push(&mut self.f.libs, id))
    }

    fn code(&mut self, origin: Origin) -> CodeIx {
        CodeIx(push(&mut self.f.codes, origin))
    }

    fn stmt(&mut self, path: &[u32]) -> StmtIx {
        let site = self.site(path);
        StmtIx(push(&mut self.f.stmts, site))
    }

    fn action(&mut self, path: &[u32], kind: ActionKind) -> ActionIx {
        let action = KAction {
            site: self.site(path),
            kind,
        };
        ActionIx(push(&mut self.f.actions, action))
    }

    fn lp(&mut self, lp: KLoop) -> LoopIx {
        LoopIx(push(&mut self.f.loops, lp))
    }

    fn tr(&mut self, t: KTry) -> TryIx {
        TryIx(push(&mut self.f.tries, t))
    }

    fn boundary(
        &mut self,
        stmt: StmtIx,
        kind: BoundaryKind,
        shape: &[CtlStatic],
        awaiting: Option<StmtIx>,
    ) -> BoundaryIx {
        let site = self.f.stmts[stmt.ix()].clone();
        self.push_boundary(site, kind, shape, awaiting)
    }

    /// A Boundary that starts no statement, at a block's site.
    fn boundary_at(&mut self, path: &[u32], kind: BoundaryKind, shape: &[CtlStatic]) -> BoundaryIx {
        let site = self.site(path);
        self.push_boundary(site, kind, shape, None)
    }

    fn push_boundary(
        &mut self,
        site: Site,
        kind: BoundaryKind,
        shape: &[CtlStatic],
        awaiting: Option<StmtIx>,
    ) -> BoundaryIx {
        let boundary = Boundary {
            site,
            kind,
            shape: ControlRecipe {
                frames: shape.into(),
            },
            awaiting,
        };
        BoundaryIx(push(&mut self.f.boundaries, boundary))
    }

    fn close(&mut self) {
        if let Some((start, exception)) = self.open.take() {
            self.blocks.push(KBlock {
                ops: OpRange {
                    start,
                    len: self.ops.len() as u32 - start,
                },
                exception,
            });
        }
    }

    fn block(&mut self, exception: Option<KBlockIx>) {
        self.close();
        self.open = Some((self.ops.len() as u32, exception));
    }

    fn op(&mut self, op: Op) {
        if let Op::Stmt { boundary } = op {
            self.stmt = self.f.boundaries[boundary.ix()].site.clone();
        }
        self.ops.push(op);
        self.origins.push(OpOrigin {
            stmt: self.stmt.clone(),
            child: Box::new([]),
        });
    }

    fn finish(mut self, name: &'static str, source: &'static str) -> Fixture {
        self.close();
        self.f.ops = self.ops.into();
        self.f.blocks = self.blocks.into();
        self.f.origins = self.origins.into();
        let maps = baseline_maps(&self.f);
        Fixture {
            name,
            source,
            function: self.f,
            maps,
        }
    }
}

/// Appends to a boxed table and returns the new entry's index.
fn push<T>(table: &mut Box<[T]>, item: T) -> u32 {
    let mut items = std::mem::take(table).into_vec();
    items.push(item);
    *table = items.into();
    table.len() as u32 - 1
}
