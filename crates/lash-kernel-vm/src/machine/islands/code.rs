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
    ($record:ident, $stream:ident, resident) => {
        $record.resident.verify($stream)?;
    };
    ($record:ident, $stream:ident, held) => {
        $record.held.verify($stream)?;
    };
    ($record:ident, $stream:ident, $other:ident) => {
        let _ = &$record.$other;
    };
}
macro_rules! check_tuple {
    (Format, $record:ident, $stream:ident) => {
        $record.value.verify($stream)?;
    };
    (Update, $record:ident, $stream:ident) => {
        $record.value.verify($stream)?;
    };
    (Access, $record:ident, $stream:ident) => {
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
            Collection,
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
            pub(in crate::machine::islands) struct Kind {
                pub(in crate::machine::islands) site: Box<KindTest>,
                pub(in crate::machine::islands) src: u32,
            }
            pub(in crate::machine::islands) struct Heap {
                pub(in crate::machine::islands) site: HeapSite,
                pub(in crate::machine::islands) args: Box<[u32]>,
                pub(in crate::machine::islands) resident: Option<HeapKernel>,
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
            pub(super) operator: Vec<sites::Operator>,
            pub(super) collection: Vec<(std::sync::Arc<CollectionStep>, bool)>,
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

        Heap {
            site: HeapSite,
            args: Box<[u32]>,
            resident: Option<HeapKernel>,
            inputs: HeapInputs,
        }
        Native {
            site: NativeSite,
            inputs: NativeInputs,
        }
        Kind {
            site: Box<KindTest>,
            input: KindInput,
        }
    }
    hot_structs {
        Constant {
            dst: u32,
            value: Value,
        }
        Local {
            dst: u32,
            at: usize,
            shared: bool,
            miss: u32,
        }
        Session {
            dst: u32,
            name: u32,
            miss: u32,
        }
        Field {
            dst: u32,
            field: usize,
            miss: u32,
        }
        Bind {
            at: usize,
            src: u32,
        }
        Return {
            reg: u32,
            pc: u32,
        }
    }
    hot_tuples {
        Accumulate (usize)
        LoopDone (u32)
        Open (usize)
        Unbind (usize)
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
                repeat: Option<Box<RepeatSite>>,
                id: StmtId,
                cost: u64,
                metered: u64,
                at: Option<usize>,
                item_hook: bool,
                otherwise: usize,
                after: usize,
            }
            LoopBranch loop_branch {
                reg: u32,
                pc: u32,
                otherwise: usize,
            }
            Depart depart {
                continuing: bool,
            }
            ReturnLiteral return_literal {
                entry: Option<usize>,
                id: StmtId,
                index: usize,
                ahead: u64,
                value: Value,
                close: Option<crate::compile::expr::Close>,
                covered: bool,
                reg: u32,
            }
        }
        [Native native Heap heap] {
            Reserve reserve {
                kind: lash_kernel_doc::ValueKind,
                members: usize,
                miss: u32,
            }
            Make make {
                pc: u32,
                ahead: u64,
                replay_guard: bool,
                make: crate::compile::expr::Make,
                items: Box<[u32]>,
                reg: u32,
                members: usize,
                miss: u32,
            }
            Record record {
                ahead: u64,
                names: Box<[Value]>,
                values: Box<[u32]>,
                reg: u32,
                members: usize,
                miss: u32,
            }
            Closure closure {
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
                cache: bool,
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
        [Kind kind] {
            Index index {
                site: Option<crate::compile::ReadSite>,
                dst: u32,
                miss: u32,
            }
            Call call {
                site: CallSite,
                read: bool,
                args: Box<[u32]>,
            }
            Cond cond {
                reg: u32,
                close: Option<crate::compile::expr::Close>,
                pc: u32,
                otherwise: usize,
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
                name: u32,
                reg: u32,
                pc: u32,
                bind: bool,
            }
        }
    }
    tuples {
        Keep keep(compiled::Restart)
        Cold cold(Deferred)
        Access access(Box<AccessSite>)
        Update update(Box<compiled::Update>)
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

pub(crate) enum KindInput {
    Register(u32),
    Source(Read),
}

pub(crate) enum HeapInputs {
    Registers,
    Read(Box<[Read; 2]>),
    Write(Box<[Read; 3]>),
}

/// Source-certified resident selection at an ordinary heap call. Arguments,
/// fallback, recovery and result installation belong to that single call.
pub(crate) enum HeapKernel {
    Read {
        units: compiled::MemberAccess,
        to: Destination,
    },
    Write {
        property: Box<compiled::MemberWrite>,
        finish: Option<u32>,
    },
}

/// Cold source roles are independent of the physical packet operands.
pub(super) struct Recovery {
    pub(super) live: Box<[u32]>,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) reason: crate::perf_witness::execution::Reason,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) library: Option<crate::compile::LibId>,
}

#[derive(Default)]
pub(super) struct Code {
    pub(super) packets: Vec<Packet>,
    pub(super) recovery: Vec<Recovery>,
    pub(super) sites: Sites,
    constants: Vec<Value>,
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
    pub(super) fn lower(ops: &mut Vec<Instruction>, blocks: &mut [Block]) -> Option<Self> {
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
                code.recovery
                    .push(Self::recovery(&instruction.op, instruction.live.clone()));
            }
            let Instruction {
                op, live, kernel, ..
            } = instruction;
            if let Some(kernel) = kernel {
                let x = u32::try_from(code.sites.collection.len()).ok()?;
                code.sites.collection.push((kernel.step, kernel.tail));
                code.packets.push(Packet {
                    opcode: Opcode::Collection,
                    mode: 0,
                    dst: 0,
                    x,
                    y: 0,
                });
                code.recovery.push(Self::recovery(&op, live.clone()));
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
            Op::Heap {
                site,
                args,
                resident,
                inputs,
            } => {
                let reads = match inputs {
                    HeapInputs::Registers => None,
                    HeapInputs::Read(reads)
                        if matches!(&resident, Some(HeapKernel::Read { .. })) =>
                    {
                        Some((*reads).into_iter().collect::<Box<[_]>>())
                    }
                    HeapInputs::Write(reads)
                        if matches!(&resident, Some(HeapKernel::Write { .. })) =>
                    {
                        Some((*reads).into_iter().collect::<Box<[_]>>())
                    }
                    _ => return None,
                };
                let x = u32::try_from(self.sites.heap.len()).ok()?;
                self.sites.heap.push(sites::Heap {
                    site,
                    args,
                    resident,
                });
                (Opcode::Heap, x, reads)
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
            Op::Kind { site, input } => {
                let (src, reads) = match input {
                    KindInput::Register(src) => (src, None),
                    KindInput::Source(read) => (read.reg(), Some(Box::new([read]) as Box<[Read]>)),
                };
                let x = u32::try_from(self.sites.kind.len()).ok()?;
                self.sites.kind.push(sites::Kind { site, src });
                (Opcode::Kind, x, reads)
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
            live,
            #[cfg(any(test, feature = "perf-witness"))]
            reason: {
                use crate::perf_witness::execution::Reason;
                match op {
                    Op::Close { .. } | Op::ReturnLiteral { .. } => Reason::Charge,
                    Op::Local { .. } | Op::Session { .. } | Op::Field { .. } | Op::Index { .. } => {
                        Reason::ResidentRead
                    }
                    Op::Operator { .. } | Op::Unit { .. } | Op::Kind { .. } | Op::Access(_) => {
                        Reason::Operator
                    }
                    Op::Native { .. } | Op::Heap { .. } => Reason::Native,
                    Op::Call { .. } => Reason::Call,
                    Op::Bind { .. } | Op::Write { .. } | Op::Store { .. } | Op::Publish { .. } => {
                        Reason::Binding
                    }
                    Op::Cond { .. } | Op::Branch { .. } | Op::LoopBranch { .. } => {
                        Reason::Condition
                    }
                    Op::Cold(Deferred::Source(_)) => Reason::ColdBranch,
                    Op::Cold(Deferred::Text { .. }) => Reason::Native,
                    _ => Reason::SourceContinuation,
                }
            },
            #[cfg(any(test, feature = "perf-witness"))]
            library: match op {
                Op::Native { site, .. } => Some(site.lib),
                Op::Operator { site, .. } => Some(site.lib),
                Op::Heap { site, .. } => Some(site.lib),
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
            Op::Constant { dst, value } => {
                let x = at(self.constants.len())?;
                self.constants.push(value);
                packet(Opcode::Constant, 0, dst, x, 0)
            }
            Op::Local {
                dst,
                at: index,
                shared,
                miss,
            } => at(index).and_then(|x| packet(Opcode::Local, u16::from(shared), dst, x, miss)),
            Op::Session { dst, name, miss } => packet(Opcode::Session, 0, dst, name, miss),
            Op::Bind { at: index, src } => {
                at(index).and_then(|dst| packet(Opcode::Bind, 0, dst, src, 0))
            }
            Op::Unbind(index) => at(index).and_then(|dst| packet(Opcode::Unbind, 0, dst, 0, 0)),
            Op::Jump(to) => at(to).and_then(|x| packet(Opcode::Jump, 0, 0, x, 0)),
            Op::Open(block) => at(block).and_then(|x| packet(Opcode::Open, 0, 0, x, 0)),
            Op::LoopDone(leaves) => packet(Opcode::LoopDone, 0, 0, leaves, 0),
            Op::Accumulate(index) => at(index).and_then(|x| packet(Opcode::Accumulate, 0, 0, x, 0)),
            Op::Return { reg, pc } => packet(Opcode::Return, 0, reg, pc, 0),
            Op::Field { dst, field, miss } => {
                at(field).and_then(|x| packet(Opcode::Field, 0, dst, x, miss))
            }
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
        self.constants[packet.x as usize]
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

    pub(super) fn operator(&self, at: usize) -> &OperatorSite {
        let packet = self.body(at);
        let at = packet.x as usize;
        match packet.opcode {
            Opcode::Operator => &self.sites.operator[at].site,
            _ => unreachable!("a verified scalar dependency names an operator site"),
        }
    }

    pub(super) fn write(&self, at: usize) -> &WriteSite {
        let packet = self.body(at);
        assert!(
            packet.opcode == Opcode::Write,
            "a verified initializer names a source write"
        );
        &self.sites.write[packet.x as usize].site
    }

    pub(super) fn access(&self, at: usize) -> &AccessSite {
        let packet = self.body(at);
        assert!(
            packet.opcode == Opcode::Access,
            "a verified access dependency names its site"
        );
        &self.sites.access[packet.x as usize].value
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
