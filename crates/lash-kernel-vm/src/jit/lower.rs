//! Lowering a compiled library code to a Cranelift function.
//!
//! The lowering follows the interpreter's steps exactly. A step is one
//! statement, or the end of a block whose entry stayed on the stack (a
//! loop's body, a `try`'s body, a function's body, or a block left by a
//! `break`). At every step boundary the function checks the slice and
//! clears what the last statement held, as `run` and `step` do; after a
//! statement completes normally it leaves the blocks that statement ended,
//! as `leave_ended_blocks` does, and after a block-end step it does not.

use std::collections::HashMap;

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{self, AbiParam, Block, InstBuilder, Signature, TrapCode, types};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, default_libcall_names};

use super::{EXIT_DEOPT, EXIT_OUTCOME, EXIT_STEP, Entry, Level};
use crate::compile::{
    BlockId, CodeId, Expr, Lib, LibRun, Member, Place, Rhs, Stmt, StmtId, Tables, Target,
};
use crate::machine::jit_rt as rt;

/// The static control stack while a statement is lowered.
#[derive(Clone, Copy, Debug)]
enum SLevel {
    Block { block: BlockId, next: u32 },
    Loop { stmt: StmtId },
    Try,
}

fn levels_of(levels: &[SLevel]) -> Vec<Level> {
    levels
        .iter()
        .map(|level| match level {
            SLevel::Block { block, next } => Level::Block {
                block: *block,
                next: *next,
            },
            SLevel::Loop { .. } | SLevel::Try => Level::Keep,
        })
        .collect()
}

pub(super) struct Lowered {
    pub(super) func: FuncId,
    pub(super) entries: HashMap<(u32, u32), u32>,
    pub(super) positions: Vec<Box<[Level]>>,
    pub(super) temps: u32,
    pub(super) code_bytes: usize,
    pub(super) statements: u32,
    pub(super) compiled_statements: u32,
}

pub(super) struct Compiler {
    module: JITModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    count: u32,
    pub(super) specialize: bool,
}

pub(super) struct Finished {
    module: Option<JITModule>,
}

impl Finished {
    pub(super) fn entry(&self, func: FuncId) -> Entry {
        let module = self.module.as_ref();
        let pointer = module.map_or(std::ptr::null(), |module| {
            module.get_finalized_function(func)
        });
        #[expect(
            unsafe_code,
            reason = "spike: a finalized function of this module has the entry ABI"
        )]
        // SAFETY: every function the compiler defines takes the context
        // pointer and returns an exit code.
        unsafe {
            std::mem::transmute::<*const u8, Entry>(pointer)
        }
    }

    pub(super) fn into_module(self) -> Option<JITModule> {
        self.module
    }
}

impl Compiler {
    #[expect(
        clippy::expect_used,
        reason = "spike: the host ISA and fixed flags are supported"
    )]
    pub(super) fn new() -> Self {
        let mut flags = settings::builder();
        flags.set("use_colocated_libcalls", "false").expect("flag");
        flags.set("is_pic", "false").expect("flag");
        flags.set("opt_level", "none").expect("flag");
        flags.set("enable_verifier", "false").expect("flag");
        let isa = cranelift_native::builder()
            .expect("a supported host")
            .finish(settings::Flags::new(flags))
            .expect("an ISA");
        let module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let ctx = module.make_context();
        Self {
            module,
            ctx,
            fctx: FunctionBuilderContext::new(),
            count: 0,
            specialize: true,
        }
    }

    pub(super) fn lower(
        &mut self,
        tables: &Tables,
        libs: &[Lib],
        code: CodeId,
        _name: &str,
    ) -> Option<Lowered> {
        self.module.clear_context(&mut self.ctx);
        let pointer = self.module.target_config().pointer_type();
        self.ctx.func.signature.params.push(AbiParam::new(pointer));
        self.ctx
            .func
            .signature
            .returns
            .push(AbiParam::new(types::I32));
        let config = self.module.target_config();
        let builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
        let mut lower = FnLower::new(builder, tables, libs, code);
        lower.specialize = self.specialize;
        lower.emit();
        let FnLower {
            builder,
            entries,
            positions,
            max_temps,
            statements,
            compiled_statements,
            ..
        } = lower;
        builder.finalize(config);
        self.count += 1;
        let signature = self.ctx.func.signature.clone();
        let func = self
            .module
            .declare_function(
                &format!("lash_jit_{}", self.count),
                Linkage::Local,
                &signature,
            )
            .ok()?;
        if let Err(error) = self.module.define_function(func, &mut self.ctx) {
            eprintln!("jit: code {} failed to compile: {error:?}", code.0);
            return None;
        }
        let code_bytes = self
            .ctx
            .compiled_code()
            .map_or(0, |compiled| compiled.code_buffer().len());
        Some(Lowered {
            func,
            entries: entries
                .into_iter()
                .enumerate()
                .map(|(index, (block, next, _))| ((block, next), index as u32))
                .collect(),
            positions,
            temps: max_temps,
            code_bytes,
            statements,
            compiled_statements,
        })
    }

    pub(super) fn finish(mut self) -> Finished {
        if self.module.finalize_definitions().is_err() {
            return Finished { module: None };
        }
        Finished {
            module: Some(self.module),
        }
    }
}

/// Which step a loop test runs in: the loop statement's own, where a loop
/// that ends leaves the blocks its statement ended, or a block-end step,
/// where it does not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Statement,
    End,
}

struct FnLower<'a, 'f> {
    builder: FunctionBuilder<'f>,
    tables: &'a Tables,
    libs: &'a [Lib],
    code: CodeId,
    ctx: ir::Value,
    charged_ptr: ir::Value,
    bound: ir::Value,
    before: ir::Value,
    slice: ir::Value,
    loop_test: ir::Value,
    dispatch: Block,
    pre: HashMap<(u32, u32), Block>,
    end: HashMap<u32, Block>,
    /// A loop's test after its body's end: what `continue` and the body's
    /// end jump to.
    test_end: HashMap<u32, Block>,
    entries: Vec<(u32, u32, Block)>,
    positions: Vec<Box<[Level]>>,
    position_of: HashMap<Vec<Level>, u32>,
    exits: HashMap<(u32, u32), Block>,
    charge_fails: HashMap<u32, Block>,
    pending: Vec<(Block, u32, u32, bool)>,
    worklist: Vec<(BlockId, Vec<SLevel>)>,
    top: u32,
    max_temps: u32,
    statements: u32,
    compiled_statements: u32,
    signatures: HashMap<(usize, bool), ir::SigRef>,
    /// Whether kernel primitives run in place (`rt_prim`).
    specialize: bool,
}

impl<'a, 'f> FnLower<'a, 'f> {
    fn new(
        mut builder: FunctionBuilder<'f>,
        tables: &'a Tables,
        libs: &'a [Lib],
        code: CodeId,
    ) -> Self {
        let prologue = builder.create_block();
        builder.append_block_params_for_function_params(prologue);
        builder.switch_to_block(prologue);
        let ctx = builder.block_params(prologue)[0];
        let flags = ir::MemFlagsData::trusted();
        let charged_ptr = builder
            .ins()
            .load(types::I64, flags, ctx, rt::OFFSET_CHARGED);
        let bound = builder.ins().load(types::I64, flags, ctx, rt::OFFSET_BOUND);
        let before = builder
            .ins()
            .load(types::I64, flags, ctx, rt::OFFSET_BEFORE);
        let slice = builder.ins().load(types::I64, flags, ctx, rt::OFFSET_SLICE);
        let loop_test = builder
            .ins()
            .load(types::I64, flags, ctx, rt::OFFSET_LOOP_TEST);
        let dispatch = builder.create_block();
        builder.ins().jump(dispatch, &[]);
        Self {
            builder,
            tables,
            libs,
            code,
            ctx,
            charged_ptr,
            bound,
            before,
            slice,
            loop_test,
            dispatch,
            pre: HashMap::new(),
            end: HashMap::new(),
            test_end: HashMap::new(),
            entries: Vec::new(),
            positions: Vec::new(),
            position_of: HashMap::new(),
            exits: HashMap::new(),
            charge_fails: HashMap::new(),
            pending: Vec::new(),
            worklist: Vec::new(),
            top: 0,
            max_temps: 0,
            statements: 0,
            compiled_statements: 0,
            signatures: HashMap::new(),
            specialize: true,
        }
    }

    fn emit(&mut self) {
        let body = self.tables.codes[self.code.0 as usize].body;
        self.worklist.push((body, Vec::new()));
        while let Some((block, parent)) = self.worklist.pop() {
            self.emit_block(block, &parent);
        }
        // The dispatch on the entry index.
        self.builder.switch_to_block(self.dispatch);
        let flags = ir::MemFlagsData::trusted();
        let entry = self
            .builder
            .ins()
            .load(types::I64, flags, self.ctx, rt::OFFSET_ENTRY);
        let otherwise = self.builder.create_block();
        let mut switch = Switch::new();
        for (index, (_, _, block)) in self.entries.iter().enumerate() {
            switch.set_entry(index as u128, *block);
        }
        switch.emit(&mut self.builder, entry, otherwise);
        self.builder.switch_to_block(otherwise);
        self.builder.ins().trap(TrapCode::unwrap_user(1));
        self.emit_exits();
        self.builder.seal_all_blocks();
    }

    // ----- blocks, positions and exits -----

    fn pre_block(&mut self, block: BlockId, index: u32) -> Block {
        if let Some(found) = self.pre.get(&(block.0, index)) {
            return *found;
        }
        let created = self.builder.create_block();
        self.pre.insert((block.0, index), created);
        created
    }

    fn end_block(&mut self, block: BlockId) -> Block {
        if let Some(found) = self.end.get(&block.0) {
            return *found;
        }
        let created = self.builder.create_block();
        self.end.insert(block.0, created);
        created
    }

    fn test_end_block(&mut self, stmt: StmtId) -> Block {
        if let Some(found) = self.test_end.get(&stmt.0) {
            return *found;
        }
        let created = self.builder.create_block();
        self.test_end.insert(stmt.0, created);
        created
    }

    fn position(&mut self, levels: &[SLevel]) -> u32 {
        let levels = levels_of(levels);
        if let Some(found) = self.position_of.get(&levels) {
            return *found;
        }
        let index = self.positions.len() as u32;
        self.positions.push(levels.clone().into_boxed_slice());
        self.position_of.insert(levels, index);
        index
    }

    /// A block that rebuilds the control stack at `levels` and leaves with
    /// `exit`. Its body is written once the function is complete.
    fn exit_block(&mut self, levels: &[SLevel], exit: u32) -> Block {
        let position = self.position(levels);
        if let Some(found) = self.exits.get(&(position, exit)) {
            return *found;
        }
        let block = self.builder.create_block();
        self.pending.push((block, position, exit, false));
        self.exits.insert((position, exit), block);
        block
    }

    /// A block that ends the run at the charge bound, then leaves as an
    /// outcome at `levels`.
    fn charge_fail_block(&mut self, levels: &[SLevel]) -> Block {
        let position = self.position(levels);
        if let Some(found) = self.charge_fails.get(&position) {
            return *found;
        }
        let block = self.builder.create_block();
        self.pending.push((block, position, EXIT_OUTCOME, true));
        self.charge_fails.insert(position, block);
        block
    }

    fn emit_exits(&mut self) {
        for (block, position, exit, charge_fail) in std::mem::take(&mut self.pending) {
            self.builder.switch_to_block(block);
            if charge_fail {
                self.call(rt::rt_charge_fail as *const () as usize, &[self.ctx], false);
            }
            let position_value = self.builder.ins().iconst(types::I64, i64::from(position));
            self.call(
                rt::rt_materialize as *const () as usize,
                &[self.ctx, position_value],
                false,
            );
            let code = self.builder.ins().iconst(types::I32, i64::from(exit));
            self.builder.ins().return_(&[code]);
        }
    }

    // ----- calls and charges -----

    fn signature(&mut self, params: usize, returns: bool) -> ir::SigRef {
        if let Some(found) = self.signatures.get(&(params, returns)) {
            return *found;
        }
        let mut signature = Signature::new(CallConv::SystemV);
        for _ in 0..params {
            signature.params.push(AbiParam::new(types::I64));
        }
        if returns {
            signature.returns.push(AbiParam::new(types::I32));
        }
        let reference = self.builder.import_signature(signature);
        self.signatures.insert((params, returns), reference);
        reference
    }

    fn call(&mut self, function: usize, args: &[ir::Value], returns: bool) -> Option<ir::Value> {
        let signature = self.signature(args.len(), returns);
        let callee = self.builder.ins().iconst(types::I64, function as i64);
        let call = self.builder.ins().call_indirect(signature, callee, args);
        returns.then(|| self.builder.inst_results(call)[0])
    }

    fn imm(&mut self, value: u64) -> ir::Value {
        self.builder.ins().iconst(types::I64, value as i64)
    }

    fn ptr<T>(&mut self, value: &T) -> ir::Value {
        self.imm(value as *const T as usize as u64)
    }

    /// Calls a runtime function that returns a status; a status that is not
    /// 0 leaves through the outcome exit at `levels`.
    fn call_checked(&mut self, function: usize, args: &[ir::Value], levels: &[SLevel]) {
        let status = self.call(function, args, true);
        let Some(status) = status else { return };
        let exit = self.exit_block(levels, EXIT_OUTCOME);
        let next = self.builder.create_block();
        self.builder.ins().brif(status, exit, &[], next, &[]);
        self.builder.switch_to_block(next);
    }

    /// Charges a constant amount, as `KernelMachine::charge` does.
    fn charge(&mut self, units: u64, levels: &[SLevel]) {
        if units == 0 {
            return;
        }
        let amount = self.imm(units);
        self.charge_value(amount, levels);
    }

    fn charge_value(&mut self, amount: ir::Value, levels: &[SLevel]) {
        let flags = ir::MemFlagsData::trusted();
        let charged = self
            .builder
            .ins()
            .load(types::I64, flags, self.charged_ptr, 0);
        let (sum, overflow) = self.builder.ins().uadd_overflow(charged, amount);
        let saturated = self.builder.ins().iconst(types::I64, -1);
        let total = self.builder.ins().select(overflow, saturated, sum);
        self.builder.ins().store(flags, total, self.charged_ptr, 0);
        let over = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, total, self.bound);
        let fail = self.charge_fail_block(levels);
        let next = self.builder.create_block();
        self.builder.ins().brif(over, fail, &[], next, &[]);
        self.builder.switch_to_block(next);
    }

    /// A step boundary: the slice `run` checks after a step, then what
    /// `step` clears before the next.
    fn boundary(&mut self, levels: &[SLevel]) {
        let flags = ir::MemFlagsData::trusted();
        let charged = self
            .builder
            .ins()
            .load(types::I64, flags, self.charged_ptr, 0);
        let spent = self.builder.ins().isub(charged, self.before);
        let sliced = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, spent, self.slice);
        let exit = self.exit_block(levels, EXIT_STEP);
        let next = self.builder.create_block();
        self.builder.ins().brif(sliced, exit, &[], next, &[]);
        self.builder.switch_to_block(next);
        self.call(rt::rt_step as *const () as usize, &[self.ctx], false);
    }

    fn temp(&mut self) -> u32 {
        let temp = self.top;
        self.top += 1;
        self.max_temps = self.max_temps.max(self.top);
        temp
    }

    // ----- statements -----

    fn emit_block(&mut self, block: BlockId, parent: &[SLevel]) {
        let stmts = self.tables.blocks[block.0 as usize].stmts.clone();
        for (index, stmt) in stmts.iter().enumerate() {
            self.emit_stmt_slot(block, index as u32, *stmt, parent);
        }
        self.emit_end(block, parent);
    }

    fn block_len(&self, block: BlockId) -> u32 {
        self.tables.blocks[block.0 as usize].stmts.len() as u32
    }

    fn with(parent: &[SLevel], level: SLevel) -> Vec<SLevel> {
        let mut levels = parent.to_vec();
        levels.push(level);
        levels
    }

    fn clear(&mut self, block: BlockId) {
        if self.tables.blocks[block.0 as usize].declares.is_empty() {
            return;
        }
        let id = self.imm(u64::from(block.0));
        self.call(rt::rt_clear as *const () as usize, &[self.ctx, id], false);
    }

    /// Where a statement step that completed normally goes: the blocks it
    /// ended are left while each sits in another block
    /// (`leave_ended_blocks`).
    fn after_statement(&mut self, parent: &[SLevel], block: BlockId, index: u32) {
        if index + 1 < self.block_len(block) {
            let target = self.pre_block(block, index + 1);
            self.builder.ins().jump(target, &[]);
            return;
        }
        if let Some((SLevel::Block { block: outer, next }, rest)) = parent.split_last() {
            self.clear(block);
            let rest = rest.to_vec();
            self.after_statement(&rest, *outer, next - 1);
            return;
        }
        let target = self.end_block(block);
        self.builder.ins().jump(target, &[]);
    }

    /// Where a block-end step goes once its block is left: nothing more is
    /// left in the same step.
    fn after_end(&mut self, block: BlockId, index: u32) {
        let target = if index + 1 < self.block_len(block) {
            self.pre_block(block, index + 1)
        } else {
            self.end_block(block)
        };
        self.builder.ins().jump(target, &[]);
    }

    fn supported(&self, stmt: &Stmt, parent: &[SLevel], block: BlockId) -> bool {
        match stmt {
            Stmt::Remove(_) | Stmt::Print(_) | Stmt::Finish(_) | Stmt::Fail(_) => false,
            Stmt::Try { finally, .. } => finally.is_none(),
            Stmt::Break | Stmt::Continue => {
                let _ = block;
                // Only blocks between the statement and its loop.
                parent
                    .iter()
                    .rev()
                    .take_while(|level| !matches!(level, SLevel::Loop { .. }))
                    .all(|level| matches!(level, SLevel::Block { .. }))
                    && parent
                        .iter()
                        .any(|level| matches!(level, SLevel::Loop { .. }))
            }
            _ => true,
        }
    }

    fn emit_stmt_slot(&mut self, block: BlockId, index: u32, id: StmtId, parent: &[SLevel]) {
        self.statements += 1;
        let pre = self.pre_block(block, index);
        self.builder.switch_to_block(pre);
        let before = Self::with(parent, SLevel::Block { block, next: index });
        self.boundary(&before);
        let stmt = &self.tables.stmts[id.0 as usize];
        if !self.supported(stmt, parent, block) {
            let exit = self.exit_block(&before, EXIT_DEOPT);
            self.builder.ins().jump(exit, &[]);
            return;
        }
        self.compiled_statements += 1;
        let start = self.builder.create_block();
        self.builder.ins().jump(start, &[]);
        self.builder.switch_to_block(start);
        self.entries.push((block.0, index, start));
        let during = Self::with(
            parent,
            SLevel::Block {
                block,
                next: index + 1,
            },
        );
        self.top = 0;
        self.charge(1, &during);
        self.emit_stmt(id, block, index, parent, &during);
    }

    fn emit_stmt(
        &mut self,
        id: StmtId,
        block: BlockId,
        index: u32,
        parent: &[SLevel],
        during: &[SLevel],
    ) {
        let tables = self.tables;
        let stmt = &tables.stmts[id.0 as usize];
        match stmt {
            Stmt::Let {
                value: Rhs::Expr(expr),
                target,
            } => {
                let value = self.temp();
                self.expr_into(expr, value, during);
                let value = self.imm(u64::from(value));
                match target {
                    Target::Slot(local) => {
                        let at = self.imm(u64::from(local.at));
                        let shared = self.imm(u64::from(local.shared));
                        self.call_checked(
                            rt::rt_bind as *const () as usize,
                            &[self.ctx, at, shared, value],
                            during,
                        );
                    }
                    Target::Session(_) => {
                        let stmt_ptr = self.ptr(stmt);
                        self.call_checked(
                            rt::rt_complete as *const () as usize,
                            &[self.ctx, stmt_ptr, value],
                            during,
                        );
                    }
                }
                self.after_statement(parent, block, index);
            }
            Stmt::Assign {
                value: Rhs::Expr(expr),
                place,
            } => {
                let value = self.temp();
                self.expr_into(expr, value, during);
                let value = self.imm(u64::from(value));
                match place {
                    Place::Var(var) => {
                        let var_ptr = self.ptr(var);
                        self.call_checked(
                            rt::rt_write_var as *const () as usize,
                            &[self.ctx, var_ptr, value],
                            during,
                        );
                    }
                    Place::Member(member) => self.write_member(member, value, during),
                }
                self.after_statement(parent, block, index);
            }
            Stmt::Let {
                value: Rhs::Action(_),
                ..
            }
            | Stmt::Assign {
                value: Rhs::Action(_),
                ..
            }
            | Stmt::Do(_) => {
                let stmt_id = self.imm(u64::from(id.0));
                let status = self
                    .call(
                        rt::rt_action as *const () as usize,
                        &[self.ctx, stmt_id],
                        true,
                    )
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I32, 0));
                let interrupted = self.builder.ins().icmp_imm_u(IntCC::Equal, status, 1);
                let exit = self.exit_block(during, EXIT_OUTCOME);
                let not_interrupted = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(interrupted, exit, &[], not_interrupted, &[]);
                self.builder.switch_to_block(not_interrupted);
                let left = self.builder.ins().icmp_imm_u(IntCC::Equal, status, 2);
                let step = self.exit_block(during, EXIT_STEP);
                let completed = self.builder.create_block();
                self.builder.ins().brif(left, step, &[], completed, &[]);
                self.builder.switch_to_block(completed);
                self.after_statement(parent, block, index);
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let value = self.temp();
                self.expr_into(condition, value, during);
                let truth = self.truth(value, 0, during);
                let then_target = self.builder.create_block();
                let else_target = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(truth, then_target, &[], else_target, &[]);
                for (target, chosen) in [(then_target, *then_block), (else_target, *else_block)] {
                    self.builder.switch_to_block(target);
                    if self.block_len(chosen) == 0 {
                        self.after_statement(parent, block, index);
                    } else {
                        let pre = self.pre_block(chosen, 0);
                        self.builder.ins().jump(pre, &[]);
                        self.worklist.push((chosen, during.to_vec()));
                    }
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                let stmt_id = self.imm(u64::from(id.0));
                self.call(
                    rt::rt_push_while as *const () as usize,
                    &[self.ctx, stmt_id],
                    false,
                );
                let looped = Self::with(during, SLevel::Loop { stmt: id });
                self.while_test(
                    id,
                    condition,
                    *body,
                    &looped,
                    parent,
                    block,
                    index,
                    Flavor::Statement,
                );
                let test_end = self.test_end_block(id);
                self.builder.switch_to_block(test_end);
                self.top = 0;
                self.while_test(
                    id,
                    condition,
                    *body,
                    &looped,
                    parent,
                    block,
                    index,
                    Flavor::End,
                );
                self.worklist.push((*body, looped));
            }
            Stmt::For { iterable, body, .. } => {
                let value = self.temp();
                self.expr_into(iterable, value, during);
                let stmt_id = self.imm(u64::from(id.0));
                let value = self.imm(u64::from(value));
                self.call_checked(
                    rt::rt_for_start as *const () as usize,
                    &[self.ctx, stmt_id, value],
                    during,
                );
                let looped = Self::with(during, SLevel::Loop { stmt: id });
                self.for_next(*body, &looped, block, index, parent, Flavor::Statement);
                let test_end = self.test_end_block(id);
                self.builder.switch_to_block(test_end);
                self.for_next(*body, &looped, block, index, parent, Flavor::End);
                self.worklist.push((*body, looped));
            }
            Stmt::Try { body, .. } => {
                let stmt_id = self.imm(u64::from(id.0));
                self.call(
                    rt::rt_push_try as *const () as usize,
                    &[self.ctx, stmt_id],
                    false,
                );
                let tried = Self::with(during, SLevel::Try);
                let target = if self.block_len(*body) == 0 {
                    self.end_block(*body)
                } else {
                    self.pre_block(*body, 0)
                };
                self.builder.ins().jump(target, &[]);
                self.worklist.push((*body, tried));
            }
            Stmt::Break | Stmt::Continue => {
                // Leave every block up to the loop, as `complete` pops them.
                let mut levels = during.to_vec();
                let mut loop_stmt = None;
                while let Some(level) = levels.pop() {
                    match level {
                        SLevel::Block { block, .. } => self.clear(block),
                        SLevel::Loop { stmt } => {
                            loop_stmt = Some(stmt);
                            break;
                        }
                        SLevel::Try => break,
                    }
                }
                let Some(loop_stmt) = loop_stmt else {
                    self.builder.ins().trap(TrapCode::unwrap_user(2));
                    return;
                };
                if matches!(stmt, Stmt::Continue) {
                    let target = self.test_end_block(loop_stmt);
                    self.builder.ins().jump(target, &[]);
                } else {
                    self.call(rt::rt_pop as *const () as usize, &[self.ctx], false);
                    // `levels` now ends with the loop's own block entry.
                    match levels.last() {
                        Some(SLevel::Block { block, next }) => {
                            let (block, next) = (*block, *next);
                            self.after_end(block, next - 1);
                        }
                        _ => {
                            self.builder.ins().trap(TrapCode::unwrap_user(3));
                        }
                    }
                }
            }
            Stmt::Return(expr) => {
                let value = self.temp();
                self.expr_into(expr, value, during);
                let value = self.imm(u64::from(value));
                self.call_checked(
                    rt::rt_return as *const () as usize,
                    &[self.ctx, value],
                    during,
                );
                let exit = self.exit_block(during, EXIT_OUTCOME);
                self.builder.ins().jump(exit, &[]);
            }
            Stmt::Throw(expr) => {
                let value = self.temp();
                self.expr_into(expr, value, during);
                let value = self.imm(u64::from(value));
                self.call(
                    rt::rt_throw as *const () as usize,
                    &[self.ctx, value],
                    false,
                );
                let exit = self.exit_block(during, EXIT_OUTCOME);
                self.builder.ins().jump(exit, &[]);
            }
            Stmt::Remove(_) | Stmt::Print(_) | Stmt::Finish(_) | Stmt::Fail(_) => {
                self.builder.ins().trap(TrapCode::unwrap_user(4));
            }
        }
    }

    /// A block's end step.
    fn emit_end(&mut self, block: BlockId, parent: &[SLevel]) {
        let end = self.end_block(block);
        self.builder.switch_to_block(end);
        let len = self.block_len(block);
        let at_end = Self::with(parent, SLevel::Block { block, next: len });
        self.boundary(&at_end);
        self.top = 0;
        self.clear(block);
        match parent.split_last() {
            None => {
                // The function's body ended: it returns null.
                self.call(rt::rt_return_null as *const () as usize, &[self.ctx], false);
                let exit = self.exit_block(&[], EXIT_OUTCOME);
                self.builder.ins().jump(exit, &[]);
            }
            Some((SLevel::Block { block: outer, next }, _)) => {
                let (outer, next) = (*outer, *next);
                self.after_end(outer, next - 1);
            }
            Some((SLevel::Loop { stmt }, _)) => {
                let target = self.test_end_block(*stmt);
                self.builder.ins().jump(target, &[]);
            }
            Some((SLevel::Try, rest)) => {
                self.call(rt::rt_pop as *const () as usize, &[self.ctx], false);
                match rest.last() {
                    Some(SLevel::Block { block: outer, next }) => {
                        let (outer, next) = (*outer, *next);
                        self.after_end(outer, next - 1);
                    }
                    _ => {
                        self.builder.ins().trap(TrapCode::unwrap_user(5));
                    }
                }
            }
        }
    }

    /// Tests a `while` loop (`next_iteration`).
    #[expect(clippy::too_many_arguments, reason = "spike")]
    fn while_test(
        &mut self,
        id: StmtId,
        condition: &Expr,
        body: BlockId,
        looped: &[SLevel],
        parent: &[SLevel],
        block: BlockId,
        index: u32,
        flavor: Flavor,
    ) {
        let _ = id;
        self.charge_value(self.loop_test, looped);
        let value = self.temp();
        self.expr_into(condition, value, looped);
        let truth = self.truth(value, 1, looped);
        let enter = self.builder.create_block();
        let leave = self.builder.create_block();
        self.builder.ins().brif(truth, enter, &[], leave, &[]);
        self.builder.switch_to_block(enter);
        self.call(rt::rt_started as *const () as usize, &[self.ctx], false);
        let target = if self.block_len(body) == 0 {
            self.end_block(body)
        } else {
            self.pre_block(body, 0)
        };
        self.builder.ins().jump(target, &[]);
        self.builder.switch_to_block(leave);
        self.call(rt::rt_pop as *const () as usize, &[self.ctx], false);
        match flavor {
            Flavor::Statement => self.after_statement(parent, block, index),
            Flavor::End => self.after_end(block, index),
        }
    }

    fn for_next(
        &mut self,
        body: BlockId,
        looped: &[SLevel],
        block: BlockId,
        index: u32,
        parent: &[SLevel],
        flavor: Flavor,
    ) {
        let status = self
            .call(rt::rt_for_next as *const () as usize, &[self.ctx], true)
            .unwrap_or_else(|| self.builder.ins().iconst(types::I32, 0));
        let interrupted = self.builder.ins().icmp_imm_u(IntCC::Equal, status, 2);
        let exit = self.exit_block(looped, EXIT_OUTCOME);
        let next = self.builder.create_block();
        self.builder.ins().brif(interrupted, exit, &[], next, &[]);
        self.builder.switch_to_block(next);
        let enter = self.builder.create_block();
        let leave = self.builder.create_block();
        self.builder.ins().brif(status, enter, &[], leave, &[]);
        self.builder.switch_to_block(enter);
        let target = if self.block_len(body) == 0 {
            self.end_block(body)
        } else {
            self.pre_block(body, 0)
        };
        self.builder.ins().jump(target, &[]);
        self.builder.switch_to_block(leave);
        match flavor {
            Flavor::Statement => self.after_statement(parent, block, index),
            Flavor::End => self.after_end(block, index),
        }
    }

    /// A condition's truth: raises unless it is a bool. `which` names the
    /// statement for the error message (0 `if`, 1 `while`).
    fn truth(&mut self, value: u32, which: u64, levels: &[SLevel]) -> ir::Value {
        let value = self.imm(u64::from(value));
        let which = self.imm(which);
        let status = self
            .call(
                rt::rt_truth as *const () as usize,
                &[self.ctx, value, which],
                true,
            )
            .unwrap_or_else(|| self.builder.ins().iconst(types::I32, 0));
        let interrupted = self.builder.ins().icmp_imm_u(IntCC::Equal, status, 2);
        let exit = self.exit_block(levels, EXIT_OUTCOME);
        let next = self.builder.create_block();
        self.builder.ins().brif(interrupted, exit, &[], next, &[]);
        self.builder.switch_to_block(next);
        status
    }

    fn write_member(&mut self, member: &Member, value: ir::Value, levels: &[SLevel]) {
        // `write_member`: the value is pinned, then the target and the
        // index are evaluated.
        self.call_checked(rt::rt_pin as *const () as usize, &[self.ctx, value], levels);
        match member {
            Member::Field(target, field) => {
                let target_temp = self.temp();
                self.expr_into(target, target_temp, levels);
                let target_value = self.imm(u64::from(target_temp));
                let field_ptr = self.ptr(field);
                self.call_checked(
                    rt::rt_set_field as *const () as usize,
                    &[self.ctx, target_value, field_ptr, value],
                    levels,
                );
            }
            Member::Index(target, index) => {
                let target_temp = self.temp();
                self.expr_into(target, target_temp, levels);
                let index_temp = self.temp();
                self.expr_into(index, index_temp, levels);
                let target_value = self.imm(u64::from(target_temp));
                let index_value = self.imm(u64::from(index_temp));
                self.call_checked(
                    rt::rt_set_index as *const () as usize,
                    &[self.ctx, target_value, index_value, value],
                    levels,
                );
            }
        }
    }

    // ----- expressions -----

    fn expr_into(&mut self, expr: &Expr, dst: u32, levels: &[SLevel]) {
        let dst_value = self.imm(u64::from(dst));
        match expr {
            Expr::Literal(value) => {
                self.charge(1, levels);
                let value_ptr = self.ptr(value);
                self.call(
                    rt::rt_lit as *const () as usize,
                    &[self.ctx, dst_value, value_ptr],
                    false,
                );
            }
            Expr::Var(var) => {
                self.charge(1, levels);
                let var_ptr = self.ptr(var);
                self.call_checked(
                    rt::rt_var as *const () as usize,
                    &[self.ctx, dst_value, var_ptr],
                    levels,
                );
            }
            Expr::Member(member) => match member.as_ref() {
                Member::Field(target, field) => {
                    self.charge(1, levels);
                    let target_temp = self.temp();
                    self.expr_into(target, target_temp, levels);
                    let target_value = self.imm(u64::from(target_temp));
                    let field_ptr = self.ptr(field);
                    self.call_checked(
                        rt::rt_field as *const () as usize,
                        &[self.ctx, dst_value, target_value, field_ptr],
                        levels,
                    );
                }
                Member::Index(target, index) => {
                    self.charge(1, levels);
                    let target_temp = self.temp();
                    self.expr_into(target, target_temp, levels);
                    let index_temp = self.temp();
                    self.expr_into(index, index_temp, levels);
                    let target_value = self.imm(u64::from(target_temp));
                    let index_value = self.imm(u64::from(index_temp));
                    self.call_checked(
                        rt::rt_index as *const () as usize,
                        &[self.ctx, dst_value, target_value, index_value],
                        levels,
                    );
                }
            },
            Expr::Call { lib, args }
                if !matches!(self.libs[lib.0 as usize].run, LibRun::Body(_)) =>
            {
                self.charge(1, levels);
                let first = self.top;
                self.top += args.len() as u32;
                self.max_temps = self.max_temps.max(self.top);
                for (offset, arg) in args.iter().enumerate() {
                    self.expr_into(arg, first + offset as u32, levels);
                }
                let lib_value = self.imm(u64::from(lib.0));
                let first_value = self.imm(u64::from(first));
                let count = self.imm(args.len() as u64);
                let function = &self.libs[lib.0 as usize];
                let direct = matches!(function.run, LibRun::Native(_))
                    && function.limit.is_none()
                    && function.arity == args.len();
                let prim = if direct && self.specialize {
                    function.prim
                } else {
                    0
                };
                if prim != 0 {
                    let op = self.imm(prim);
                    self.call_checked(
                        rt::rt_prim as *const () as usize,
                        &[self.ctx, dst_value, lib_value, first_value, op],
                        levels,
                    );
                } else {
                    let entry = if direct {
                        rt::rt_native_direct as *const () as usize
                    } else {
                        rt::rt_native as *const () as usize
                    };
                    self.call_checked(
                        entry,
                        &[self.ctx, dst_value, lib_value, first_value, count],
                        levels,
                    );
                }
            }
            Expr::List(items) | Expr::Tuple(items) => {
                self.charge(1, levels);
                let first = self.top;
                self.top += items.len() as u32;
                self.max_temps = self.max_temps.max(self.top);
                for (offset, item) in items.iter().enumerate() {
                    self.expr_into(item, first + offset as u32, levels);
                }
                self.charge(items.len() as u64, levels);
                let first_value = self.imm(u64::from(first));
                let count = self.imm(items.len() as u64);
                let function = if matches!(expr, Expr::List(_)) {
                    rt::rt_list as *const () as usize
                } else {
                    rt::rt_tuple as *const () as usize
                };
                self.call_checked(function, &[self.ctx, dst_value, first_value, count], levels);
            }
            Expr::Record(entries) => {
                self.charge(1, levels);
                let first = self.top;
                self.top += entries.len() as u32;
                self.max_temps = self.max_temps.max(self.top);
                for (offset, (_, value)) in entries.iter().enumerate() {
                    self.expr_into(value, first + offset as u32, levels);
                }
                self.charge(entries.len() as u64, levels);
                let first_value = self.imm(u64::from(first));
                let expr_ptr = self.ptr(expr);
                self.call_checked(
                    rt::rt_record as *const () as usize,
                    &[self.ctx, dst_value, first_value, expr_ptr],
                    levels,
                );
            }
            _ => {
                // Everything else the interpreter evaluates, charge
                // included.
                let expr_ptr = self.ptr(expr);
                self.call_checked(
                    rt::rt_eval as *const () as usize,
                    &[self.ctx, dst_value, expr_ptr],
                    levels,
                );
            }
        }
    }
}
