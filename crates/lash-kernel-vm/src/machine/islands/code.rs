//! Compact packets and immutable source recovery. Compiler operations are
//! consumed at publication; the runtime never retains or decodes that enum.

use super::compiled::*;
use super::*;

use crate::compile::{CodeId, StmtId};

/// Fixed-width safe Rust encoding. Each index is checked during publication.
#[derive(Clone, Copy)]
pub(super) struct Packet {
    pub(super) opcode: Opcode,
    pub(super) mode: u16,
    pub(super) dst: u32,
    pub(super) x: u32,
    pub(super) y: u32,
}

const _: () = assert!(std::mem::size_of::<Packet>() == 16);

mod families;
pub(super) mod repeat;
mod verify;
use verify::Closes;

macro_rules! check_field {
    ($record:ident, $stream:ident, literal_prefix) => {
        if !$record.source.verify($stream, $record.reg)
            || !$record.literal_prefix.matches_constructor(
                $stream,
                $record.source.pc(),
                $record.source.unpaid(),
                $record.source.quote(),
            )
        {
            return Err(crate::compile::islands::Refused::Region {
                region: $record.source.pc(),
            });
        }
    };
    ($record:ident, $stream:ident, original) => {
        if !$record
            .original
            .matches_constructor($stream, $record.pc, $record.miss, $record.ahead)
        {
            return Err(crate::compile::islands::Refused::Region { region: $record.pc });
        }
    };
    ($record:ident, $stream:ident, close) => {
        $record.close.verify($stream)?;
    };
    ($record:ident, $stream:ident, site) => {
        $record.site.verify($stream)?;
    };
    ($record:ident, $stream:ident, to) => {
        $record.to.verify($stream)?;
    };
    ($record:ident, $stream:ident, write) => {
        $record.write.verify($stream)?;
    };
    ($record:ident, $stream:ident, held) => {
        $record.held.verify($stream)?;
    };
    ($record:ident, $stream:ident, text_inputs) => {
        if let NativeInputs::Source(reads) = &$record.text_inputs {
            for read in reads {
                if !read.source().verify($stream, read.reg()) {
                    return Err(crate::compile::islands::Refused::Region {
                        region: read.source().pc(),
                    });
                }
            }
        }
    };
    ($record:ident, $stream:ident, $other:ident) => {
        let _ = &$record.$other;
    };
}
macro_rules! check_tuple {
    (Certified, $record:ident, $stream:ident) => {
        $record
            .value
            .verify($stream)
            .map_err(crate::compile::islands::Refused::Room)?;
    };
    (Format, $record:ident, $stream:ident) => {
        $record.value.verify($stream)?;
    };
    ($other:ident, $record:ident, $stream:ident) => {
        let _ = &$record.value;
    };
}

macro_rules! operations {
    (
        inputs { $( $input:ident { $( $input_field:ident: $input_ty:ty ),* $(,)? } )* }
        hot_structs { $( $hot:ident { $( $hot_field:ident: $hot_ty:ty ),* $(,)? } )* }
        hot_tuples { $( $hot_tuple:ident ( $hot_tuple_ty:ty ) )* }
        structs { $( [ $( $manual:ident $manual_table:ident )* ] { $( $name:ident $table:ident { $( $field:ident: $ty:ty ),* $(,)? } )* } )* }
        tuples { $( $tuple:ident $tuple_table:ident ( $tuple_ty:ty ) )* }
        units { $( $unit:ident )* }
    ) => {
        /// Lowering IR. No value of this type survives Code::lower.
        pub(crate) enum Op {
            $( $input { $( $input_field: $input_ty ),* }, )*
            $( $hot { $( $hot_field: $hot_ty ),* }, )*
            $( $hot_tuple($hot_tuple_ty), )*
            $( $( $name { $( $field: $ty ),* }, )* )*
            $( $tuple($tuple_ty), )*
            $( $unit, )*
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr(u16)]
        pub(super) enum Opcode {
            Begin,
            SourcePublication,
            CollectionAppend,
            CollectionReduce,
            CollectionPlacement,
            CollectionVisit,
            CollectionReadSlot,
            CollectionMerge,
            CollectionMergeTail,
            Operator,
            $( $hot, )*
            $( $hot_tuple, )*
            $( $( $manual, )* $( $name, )* )*
            $( $tuple, )*
            $( $unit, )*
        }

        pub(super) mod sites {
            use super::*;
            pub(in crate::machine::islands) struct Operator {
                pub(in crate::machine::islands) site: Box<OperatorSite>,
                pub(in crate::machine::islands) inputs: OperatorInputs,
                pub(in crate::machine::islands) covered: bool,
                pub(in crate::machine::islands) to: Destination,
            }
            pub(in crate::machine::islands) struct Native {
                pub(in crate::machine::islands) site: NativeSite,
                pub(in crate::machine::islands) args: Box<[u32]>,
            }
            $( $( pub(in crate::machine::islands) struct $name {
                $( pub(in crate::machine::islands) $field: $ty, )*
            } )* )*
            $( pub(in crate::machine::islands) struct $tuple {
                pub(in crate::machine::islands) value: $tuple_ty,
            } )*
        }

        #[derive(Default)]
        pub(super) struct Sites {
            pub(super) begin: Vec<StatementEntry>,
            pub(super) source_publications: Vec<super::execute::source::SourcePublication>,
            pub(super) operator: Vec<sites::Operator>,
            pub(super) collection_append: Vec<CollectionPacket<CollectionAppend>>,
            pub(super) collection_reduce: Vec<CollectionPacket<Reduction>>,
            pub(super) collection_placement: Vec<CollectionPacket<Placement>>,
            pub(super) collection_visit: Vec<CollectionPacket<Visit>>,
            pub(super) collection_read: Vec<CollectionPacket<ReadSlot>>,
            pub(super) collection_merge: Vec<CollectionPacket<Merge>>,
            pub(super) collection_tail: Vec<CollectionPacket<super::execute::MergeTail>>,
            $( $( pub(super) $manual_table: Vec<sites::$manual>, )*
               $( pub(super) $table: Vec<sites::$name>, )* )*
            $( pub(super) $tuple_table: Vec<sites::$tuple>, )*
        }

        impl Code {
            pub(in crate::machine::islands) fn verify(&self, stream: &Stream) -> Result<(), crate::compile::islands::Refused> {
                $( $( for record in &self.sites.$manual_table {
                    record.verify(stream)?;
                } )*
                $( for record in &self.sites.$table {
                    $( check_field!(record, stream, $field); )*
                } )* )*
                $( for record in &self.sites.$tuple_table {
                    check_tuple!($tuple, record, stream);
                } )*
                for record in &self.sites.operator {
                    record.site.verify(stream)?;
                    record.to.verify(stream)?;
                }
                for operation in &self.sites.cold {
                    if let Deferred::Text { dst, constant, source, original } = &operation.value
                        && (!original.matches_constructor(stream, source.pc(), source.unpaid(), source.quote())
                            || !source.verify(stream, *dst)
                            || !matches!(stream.ops.get(source.pc() as usize), Some(lash_kernel_exec::Op::Const { c, .. }) if c == constant)
                            || !matches!(stream.consts.get(constant.ix()), Some(crate::compile::Constant::Deferred(lash_kernel_doc::Literal::Text(_)))))
                    {
                            return Err(crate::compile::islands::Refused::Region { region: source.pc() });
                    }
                }
                for (packet, recovery) in self.packets.iter().zip(&self.recovery) {
                    if let Some(source) = recovery.source
                        && (!matches!(packet.opcode, Opcode::Constant | Opcode::Local | Opcode::Session)
                            || !source.verify(stream, packet.dst)
                            || !match (packet.opcode, &stream.ops[source.pc() as usize]) {
                                (Opcode::Constant, lash_kernel_exec::Op::Const { c, .. }) => {
                                    matches!(&stream.consts[c.ix()], crate::compile::Constant::Resident(value) if self.constant(*packet).same(value))
                                }
                                (Opcode::Local, lash_kernel_exec::Op::Local { local, .. }) => packet.x == local.at,
                                (Opcode::Session, lash_kernel_exec::Op::Session { name, .. }) => packet.x == name.0,
                                _ => false,
                            }) {
                        return Err(crate::compile::islands::Refused::Region { region: source.pc() });
                    }
                }
                for reads in &self.inputs {
                    for read in reads {
                        if !read.source().verify(stream, read.reg()) {
                            return Err(crate::compile::islands::Refused::Region { region: read.source().pc() });
                        }
                    }
                }
                for format in &self.sites.format {
                    let record = &format.value.parts;
                    check_field!(record, stream, original);
                }
                self.verify_roles(stream)?;
                self.verify_constructions(stream)
            }

            fn encode(&mut self, op: Op) -> Option<Packet> {
                if matches!(&op, $( Op::$input { .. } )|*) {
                    return self.input_packet(op);
                }
                match &op {
                    $( Op::$hot { .. } => return self.hot(&op), )*
                    $( Op::$hot_tuple(_) => return self.hot(&op), )*
                    _ => {}
                }
                let (opcode, x) = match op {
                    $( $( Op::$name { $( $field ),* } => {
                        let at = u32::try_from(self.sites.$table.len()).ok()?;
                        self.sites.$table.push(sites::$name { $( $field ),* });
                        (Opcode::$name, at)
                    }, )* )*
                    $( Op::$tuple(value) => {
                        let at = u32::try_from(self.sites.$tuple_table.len()).ok()?;
                        self.sites.$tuple_table.push(sites::$tuple { value });
                        (Opcode::$tuple, at)
                    }, )*
                    $( Op::$unit => (Opcode::$unit, 0), )*
                    _ => unreachable!("hot operations were encoded before site publication"),
                };
                Some(Packet { opcode, mode: 0, dst: 0, x, y: 0 })
            }
        }
    };
}

operations! {
    inputs {
        Operator {
            site: Box<OperatorSite>,
            inputs: OperatorInputs,
            covered: bool,
            to: Destination,
        }

        Native {
            site: NativeSite,
            inputs: NativeInputs,
        }
    }
    hot_structs {
        Constant {
            dst: u32,
            value: Value,
            source: Option<crate::compile::expr::SourceRead>,
            original: Option<std::sync::Arc<crate::compile::room::charges::OriginalPrefix>>,
        }
        Local {
            dst: u32,
            at: usize,
            shared: bool,
            miss: u32,
            source: Option<crate::compile::expr::SourceRead>,
        }
        Session {
            dst: u32,
            name: u32,
            miss: u32,
            source: Option<crate::compile::expr::SourceRead>,
        }
        Return {
            reg: u32,
            pc: u32,
        }
    }
    hot_tuples {
        LoopDone (u32)
        Open (usize)
        Jump (usize)
        Effect (u32)
        Spawn (u32)
        Yield (u32)
        Join (u32)
        Wait (u32)
        Cancel (u32)
    }
    structs {
        [] {
            Depart depart {
                pc: u32,
                continuing: bool,
            }
            Try try_ {
                id: StmtId,
                finally: Option<usize>,
                after: usize,
            }
            Loop loop_ {
                id: StmtId,
                header: usize,
                texts: Box<[usize]>,
            }
            For for_ {
                id: StmtId,
                header: usize,
                reg: u32,
                pc: u32,
                via: bool,
                texts: Box<[usize]>,
            }
            Header header {
                entry: Option<std::sync::Arc<crate::compile::room::publication::boundary::LoopEntry>>,
                repeat: Option<Box<RepeatSite>>,
                id: StmtId,
                cost: u64,
                metered: u64,
                at: Option<usize>,
                otherwise: usize,
                after: usize,
            }
            LoopBranch loop_branch {
                entry: Option<std::sync::Arc<crate::compile::room::publication::boundary::LoopEntry>>,
                reg: u32,
                pc: u32,
                otherwise: usize,
            }
            ReturnLiteral return_literal {
                entry: Option<usize>,
                id: StmtId,
                index: usize,
                ahead: u64,
                value: Value,
                close: Option<crate::compile::expr::Close>,
                source: crate::compile::expr::SourceRead,
                literal_prefix: std::sync::Arc<crate::compile::room::charges::OriginalPrefix>,
                reg: u32,
            }
        }
        [Native native] {
            TextWrite text_write {
                site: NativeSite,
                text_inputs: NativeInputs,
                write: Box<WriteSite>,
            }
            Reserve reserve {
                kind: lash_kernel_doc::ValueKind,
                members: usize,
                miss: u32,
            }
            Make make {
                pc: u32,
                ahead: u64,
                original: std::sync::Arc<crate::compile::room::charges::OriginalPrefix>,
                make: crate::compile::expr::Make,
                items: Box<[u32]>,
                reg: u32,
                members: usize,
                miss: u32,
            }
            Record record {
                pc: u32,
                ahead: u64,
                original: std::sync::Arc<crate::compile::room::charges::OriginalPrefix>,
                names: Box<[Value]>,
                values: Box<[u32]>,
                reg: u32,
                members: usize,
                miss: u32,
            }
            Closure closure {
                pc: u32,
                ahead: u64,
                original: std::sync::Arc<crate::compile::room::charges::OriginalPrefix>,
                code: CodeId,
                reg: u32,
                miss: u32,
            }
            Store store {
                target: u32,
                index: Option<u32>,
                value: u32,
                field: Option<usize>,
                pc: u32,
                inputs: Option<Box<StoreInputs>>,
                finish: Option<u32>,
            }
            Remove remove {
                target: u32,
                index: u32,
                pc: u32,
            }
            Unit unit {
                site: UnitSite,
                src: u32,
            }
        }
        [] {
            Field field {
                dst: u32,
                field: usize,
                miss: u32,
                pc: u32,
            }
            Index index {
                site: Option<crate::compile::ReadSite>,
                dst: u32,
                miss: u32,
                pc: u32,
            }
            Call call {
                site: CallSite,
                read: bool,
                args: Box<[u32]>,
            }
            Close close {
                close: crate::compile::expr::Close,
                covered: bool,
            }
            Write write {
                site: Box<WriteSite>,
            }
            Done done {
                leaves: u32,
                empty: Option<crate::compile::BlockId>,
            }
            Branch branch {
                reg: u32,
                pc: u32,
                otherwise: usize,
                opens: [Option<usize>; 2],
            }
            Publish publish {
                charges: Option<std::sync::Arc<crate::compile::room::charges::ChargeProgram>>,
                name: u32,
                reg: u32,
                pc: u32,
                bind: bool,
            }
        }
    }
    tuples {
        Certified certified(Box<compiled::resource::CertifiedStatement>)
        Cold cold(Deferred)
        Format format(Box<compiled::Format>)
    }
    units { EndLoop }
}

/// Fixed scalar placements use the canonical operator packet and library
/// kernel facts. They carry no second runtime site or copied native kernel.
pub(crate) enum OperatorInputs {
    Registers(Box<[u32]>),
    Source(Box<[Read]>),
    Scalar(ScalarInputs),
}

pub(crate) enum ScalarInputs {
    Unary(Operand),
    Binary([Operand; 2]),
}

pub(crate) enum NativeInputs {
    Registers(Box<[u32]>),
    Source(Box<[Read]>),
}

/// Cold source roles are independent of the physical packet operands.
pub(super) struct Recovery {
    pub(super) source: Option<crate::compile::expr::SourceRead>,
    pub(super) live: Box<[u32]>,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) reason: crate::perf_witness::execution::Reason,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) library: Option<crate::compile::LibId>,
}

struct ConstantValue {
    value: Value,
    original: Option<std::sync::Arc<crate::compile::room::charges::OriginalPrefix>>,
}
#[derive(Default)]
pub(super) struct Code {
    pub(super) packets: Vec<Packet>,
    pub(super) recovery: Vec<Recovery>,
    pub(super) sites: Sites,
    constants: Vec<ConstantValue>,
    /// Pure input placements select a mode of the canonical operation.
    pub(super) inputs: Vec<Box<[Read]>>,
}

/// Every compiler selection and final publication uses this address map.
/// Source recovery coordinates are carried by the descriptors, never decoded
/// from a removed predecessor operation.
pub(super) fn relocate(ops: &mut [Instruction], blocks: &mut [Block], mapping: &[usize]) {
    for instruction in ops {
        instruction.op.addresses(&mut |at| *at = mapping[*at]);
        if let Some(kernel) = &mut instruction.kernel {
            kernel.addresses(&mut |at| *at = mapping[*at]);
        }
    }
    for block in blocks {
        for start in &mut block.starts {
            *start = mapping[*start];
        }
        block.end = mapping[block.end];
        block.after = mapping[block.after];
    }
}

impl Code {
    pub(super) fn lower(
        ops: &mut Vec<Instruction>,
        blocks: &mut [Block],
        fused: bool,
    ) -> Option<Self> {
        let mut relocation = Vec::with_capacity(ops.len() + 1);
        let mut length = 0usize;
        for instruction in ops.iter() {
            relocation.push(length);
            length = length.checked_add(
                1 + usize::from(instruction.begin.is_some())
                    + usize::from(instruction.kernel.is_some()),
            )?;
        }
        relocation.push(length);
        u32::try_from(length).ok()?;
        relocate(ops, blocks, &relocation);
        let mut code = Self::default();
        for instruction in std::mem::take(ops) {
            if let Some(begin) = instruction.begin {
                let x = u32::try_from(code.sites.begin.len()).ok()?;
                code.sites.begin.push(begin);
                code.packets.push(Packet {
                    opcode: Opcode::Begin,
                    mode: 0,
                    dst: 0,
                    x,
                    y: 0,
                });
                let mut recovery = Self::recovery(&instruction.op, instruction.live.clone());
                recovery.source = None;
                code.recovery.push(recovery);
            }
            let Instruction {
                op, live, kernel, ..
            } = instruction;
            if let Some(kernel) = kernel {
                fn insert<S>(sites: &mut Vec<CollectionPacket<S>>, shape: S) -> Option<u32> {
                    let at = u32::try_from(sites.len()).ok()?;
                    sites.push(CollectionPacket::new(shape));
                    Some(at)
                }
                let (opcode, x) = match (&*kernel.step, kernel.tail) {
                    (CollectionStep::Append(shape), false) => (
                        Opcode::CollectionAppend,
                        insert(&mut code.sites.collection_append, shape.clone())?,
                    ),
                    (CollectionStep::Reduce(shape), false) => (
                        Opcode::CollectionReduce,
                        insert(&mut code.sites.collection_reduce, shape.clone())?,
                    ),
                    (CollectionStep::Place(shape), false) => (
                        Opcode::CollectionPlacement,
                        insert(&mut code.sites.collection_placement, shape.clone())?,
                    ),
                    (CollectionStep::Visit(shape), false) => (
                        Opcode::CollectionVisit,
                        insert(&mut code.sites.collection_visit, shape.clone())?,
                    ),
                    (CollectionStep::ReadSlot(shape), false) => (
                        Opcode::CollectionReadSlot,
                        insert(&mut code.sites.collection_read, shape.clone())?,
                    ),
                    (CollectionStep::Merge(shape), false) => (
                        Opcode::CollectionMerge,
                        insert(&mut code.sites.collection_merge, shape.clone())?,
                    ),
                    (CollectionStep::Merge(shape), true) => (
                        Opcode::CollectionMergeTail,
                        insert(
                            &mut code.sites.collection_tail,
                            super::execute::MergeTail::new(shape.clone()),
                        )?,
                    ),
                    _ => return None,
                };
                let (opcode, x) = if fused {
                    (opcode, x)
                } else {
                    let source = super::execute::source::SourcePublication::new(
                        &kernel.step,
                        kernel.tail,
                        x,
                    )?;
                    let at = u32::try_from(code.sites.source_publications.len()).ok()?;
                    code.sites.source_publications.push(source);
                    (Opcode::SourcePublication, at)
                };
                code.packets.push(Packet {
                    opcode,
                    mode: 0,
                    dst: 0,
                    x,
                    y: 0,
                });
                let mut recovery = Self::recovery(&op, live.clone());
                recovery.source = None;
                code.recovery.push(recovery);
            }
            let scheduler = op.scheduler_pc();
            let recovery = Self::recovery(&op, live);
            let mut packet = code.encode(op)?;
            if let Some(pc) = scheduler {
                packet.y = pc;
            }
            code.packets.push(packet);
            code.recovery.push(recovery);
        }
        Some(code)
    }

    /// An evaluated text native resumes the same TextWrite packet. Input reads
    /// have already published, so this entry uses their authenticated registers.
    pub(super) fn text_suffixes(&mut self, stream: &Stream) -> Option<Vec<(u32, usize)>> {
        let mut suffixes = Vec::new();
        for (at, packet) in self.packets.iter().enumerate() {
            let pair = match packet.opcode {
                Opcode::TextWrite => {
                    let entry = &self.sites.text_write[packet.x as usize];
                    Some((entry.site.clone(), entry.write.clone()))
                }
                Opcode::Operator => {
                    let entry = &self.sites.operator[packet.x as usize];
                    if let Destination::Write(write) = &entry.to {
                        Some((
                            NativeSite {
                                lib: entry.site.lib,
                                close: entry.site.close,
                                charges: entry.site.charges.clone(),
                                pc: entry.site.pc,
                                reg: entry.site.reg,
                                depth: entry.site.depth,
                            },
                            write.clone(),
                        ))
                    } else {
                        None
                    }
                }
                Opcode::Format => {
                    let entry = &self.sites.format[packet.x as usize].value;
                    Some((
                        NativeSite {
                            lib: entry.append.lib,
                            close: entry.append.close,
                            charges: entry.append.charges.clone(),
                            pc: entry.append.pc,
                            reg: entry.append.reg,
                            depth: entry.append.depth,
                        },
                        entry.write.clone(),
                    ))
                }
                Opcode::Write => {
                    let write = &self.sites.write[packet.x as usize].site;
                    if let Some(plan) = &write.text {
                        self.sites
                            .native
                            .iter()
                            .find(|entry| entry.site.pc == plan.native_pc())
                            .map(|entry| (entry.site.clone(), write.clone()))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((site, write)) = pair
                && write.text.is_some()
            {
                let op = stream.ops.get(site.pc as usize)?;
                let inputs: Box<[_]> = op.reads().iter().map(|reg| reg.0).collect();
                suffixes.push((site, write, inputs, at + 1));
            }
        }
        let mut entries = Vec::new();
        for (site, write, inputs, after) in suffixes {
            let pc = site.pc;
            let live = inputs.clone();
            let op = Op::TextWrite {
                site,
                text_inputs: NativeInputs::Registers(inputs),
                write,
            };
            let at = self.packets.len();
            let recovery = Self::recovery(&op, live);
            let packet = self.encode(op)?;
            self.packets.push(packet);
            self.recovery.push(recovery);
            let op = Op::Jump(after);
            let recovery = Self::recovery(&op, Box::new([]));
            let packet = self.encode(op)?;
            self.packets.push(packet);
            self.recovery.push(recovery);
            entries.push((pc, at));
        }
        entries.sort_unstable_by_key(|(pc, _)| *pc);
        Some(entries)
    }

    /// Compiler input facts select the same semantic site and decoder as
    /// register operands. No direct-input operation survives publication.
    fn input_packet(&mut self, op: Op) -> Option<Packet> {
        let (opcode, x, reads) = match op {
            Op::Operator {
                site,
                inputs,
                covered,
                to,
            } => {
                let x = u32::try_from(self.sites.operator.len()).ok()?;
                self.sites.operator.push(sites::Operator {
                    site,
                    inputs,
                    covered,
                    to,
                });
                (Opcode::Operator, x, None)
            }
            Op::Native { site, inputs } => {
                let (args, reads) = match inputs {
                    NativeInputs::Registers(args) => (args, None),
                    NativeInputs::Source(reads) => (Box::default(), Some(reads)),
                };
                let x = u32::try_from(self.sites.native.len()).ok()?;
                self.sites.native.push(sites::Native { site, args });
                (Opcode::Native, x, reads)
            }
            _ => unreachable!("only compiler input facts select an input mode"),
        };
        let (mode, y) = match reads {
            Some(reads) => {
                let y = u32::try_from(self.inputs.len()).ok()?;
                self.inputs.push(reads);
                (1, y)
            }
            None => (0, 0),
        };
        Some(Packet {
            opcode,
            mode,
            dst: 0,
            x,
            y,
        })
    }

    fn recovery(op: &Op, live: Box<[u32]>) -> Recovery {
        #[cfg(not(any(test, feature = "perf-witness")))]
        let _ = op;
        Recovery {
            source: match op {
                Op::Constant { source, .. }
                | Op::Local { source, .. }
                | Op::Session { source, .. } => *source,
                _ => None,
            },
            live,
            #[cfg(any(test, feature = "perf-witness"))]
            reason: {
                use crate::perf_witness::execution::Reason;
                match op {
                    Op::Close { .. } | Op::ReturnLiteral { .. } => Reason::Charge,
                    Op::Local { .. } | Op::Session { .. } | Op::Field { .. } | Op::Index { .. } => {
                        Reason::ResidentRead
                    }
                    Op::Operator { .. } | Op::Unit { .. } => Reason::Operator,
                    Op::Native { .. } | Op::TextWrite { .. } => Reason::Native,
                    Op::Call { .. } => Reason::Call,
                    Op::Write { .. } | Op::Store { .. } | Op::Publish { .. } => Reason::Binding,
                    Op::Branch { .. } | Op::LoopBranch { .. } => Reason::Condition,
                    Op::Cold(Deferred::Source(_)) => Reason::ColdBranch,
                    Op::Cold(Deferred::Text { .. }) => Reason::Native,
                    _ => Reason::SourceContinuation,
                }
            },
            #[cfg(any(test, feature = "perf-witness"))]
            library: match op {
                Op::Native { site, .. } | Op::TextWrite { site, .. } => Some(site.lib),
                Op::Operator { site, .. } => Some(site.lib),
                Op::Call { site, .. } => site.library,
                _ => None,
            },
        }
    }

    fn hot(&mut self, op: &Op) -> Option<Packet> {
        let packet = |opcode, mode, dst, x, y| {
            Some(Packet {
                opcode,
                mode,
                dst,
                x,
                y,
            })
        };
        let at = |n: usize| u32::try_from(n).ok();
        match *op {
            Op::Constant {
                dst,
                value,
                ref original,
                ..
            } => {
                let x = at(self.constants.len())?;
                self.constants.push(ConstantValue {
                    value,
                    original: original.clone(),
                });
                packet(Opcode::Constant, 0, dst, x, 0)
            }
            Op::Local {
                dst,
                at: index,
                shared,
                miss,
                ..
            } => at(index).and_then(|x| packet(Opcode::Local, u16::from(shared), dst, x, miss)),
            Op::Session {
                dst, name, miss, ..
            } => packet(Opcode::Session, 0, dst, name, miss),
            Op::Jump(to) => at(to).and_then(|x| packet(Opcode::Jump, 0, 0, x, 0)),
            Op::Open(block) => at(block).and_then(|x| packet(Opcode::Open, 0, 0, x, 0)),
            Op::LoopDone(leaves) => packet(Opcode::LoopDone, 0, 0, leaves, 0),
            Op::Return { reg, pc } => packet(Opcode::Return, 0, reg, pc, 0),
            Op::Effect(pc) => packet(Opcode::Effect, 0, 0, pc, 0),
            Op::Spawn(pc) => packet(Opcode::Spawn, 0, 0, pc, pc),
            Op::Yield(pc) => packet(Opcode::Yield, 0, 0, pc, pc),
            Op::Join(pc) => packet(Opcode::Join, 0, 0, pc, pc),
            Op::Wait(pc) => packet(Opcode::Wait, 0, 0, pc, pc),
            Op::Cancel(pc) => packet(Opcode::Cancel, 0, 0, pc, pc),
            _ => unreachable!("only inline operations enter the hot encoder"),
        }
    }

    pub(super) fn constant(&self, packet: Packet) -> Value {
        self.constants[packet.x as usize].value
    }
    pub(super) fn constant_original(
        &self,
        packet: Packet,
    ) -> Option<&std::sync::Arc<crate::compile::room::charges::OriginalPrefix>> {
        self.constants[packet.x as usize].original.as_ref()
    }

    /// Metadata readers name the body packet even when the source entry
    /// has an explicit admission packet immediately before it.
    pub(super) fn body(&self, mut at: usize) -> Packet {
        if self.packets[at].opcode == Opcode::Begin {
            at += 1;
        }
        self.packets[at]
    }

    pub(super) fn entry(&self, at: usize) -> Option<&StatementEntry> {
        let packet = self.packets[at];
        (packet.opcode == Opcode::Begin).then(|| &self.sites.begin[packet.x as usize])
    }

    pub(super) fn loop_exit(&self, at: usize) -> usize {
        let packet = self.body(at);
        assert!(
            packet.opcode == Opcode::Header,
            "a loop continuation names its header"
        );
        self.sites.header[packet.x as usize].after
    }
}
