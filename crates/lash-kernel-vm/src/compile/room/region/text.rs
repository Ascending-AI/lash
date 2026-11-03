//! A forwarding text body uses source lifetime algebra and the installed native.
use super::*;
#[path = "../publication/text/body.rs"]
mod prepared;
use crate::compile::{self, CodeId, Executable, LibId, LibRun};

pub(crate) struct TextBody {
    library: LibId,
    code: CodeId,
    primitive: LibId,
    region: ScalarRegion,
    parameters: Box<[Slot]>,
}
pub(crate) struct TextBodyCall {
    room: CheckedRoom,
    dependencies: logical::Dependencies,
    reservation: logical::InvocationRange,
    caller: logical::Invocation,
    depth: u64,
}
struct Route<'a> {
    source: &'a Source,
    selected: Vec<Selection>,
}
impl evaluate::Events for Route<'_> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let at = Point {
            site: site.clone(),
            phase,
        };
        let exit = self
            .source
            .room
            .exits
            .iter()
            .find(|exit| exit.point == at)
            .ok_or(Refused::Source)?
            .kind;
        self.selected.push(Selection::Event { at, exit });
        Ok(exit)
    }
    fn choose(&mut self, _: &Site) -> Result<bool, Refused> {
        Err(Refused::Source)
    }
}
impl TextBody {
    pub(crate) fn compile(exe: &Executable, library: LibId) -> Result<Self, Refused> {
        let function = exe.lib(library);
        let LibRun::Body(code) = function.run else {
            return Err(Refused::Source);
        };
        let body = exe.code(code);
        let source = &body.source_room;
        if function.arity != 2
            || function.limit.is_some()
            || function.shares_params
            || body.charging
            || body.params.len() != 2
            || !body.captures.is_empty()
            || !body.free.is_empty()
            || body.slots.iter().any(|slot| slot.shared)
            || function.specialization.resource
                != CallSource::Body(function.specialization.function)
            || source.owner
                != doc::constant_pool::PoolOwner::Function(function.specialization.function)
        {
            return Err(Refused::Source);
        }
        let [
            doc::Stmt::Return {
                value:
                    doc::Expr::Call {
                        function: target,
                        args,
                    },
            },
        ] = &**source.body
        else {
            return Err(Refused::Source);
        };
        if args.len() != source.params.len()
            || !args
                .iter()
                .zip(&source.params)
                .all(|(arg, name)| matches!(arg, doc::Expr::Variable(actual) if actual == name))
        {
            return Err(Refused::Source);
        }
        let primitive = exe.library_lib(target).ok_or(Refused::Source)?;
        let native = exe.lib(primitive);
        if !matches!(native.run, LibRun::Native { .. })
            || !native.specialization.native.text_append
            || native.arity != 2
            || native.limit.is_some()
            || native.shares_params
            || native.specialization.resource != CallSource::Native(native.specialization.function)
        {
            return Err(Refused::Source);
        }
        let mut route = Route {
            source,
            selected: Vec::new(),
        };
        if evaluate::walk(source, &source.site, &mut route, 4096)? != ExitKind::Return {
            return Err(Refused::Source);
        }
        let interval = source.compile_interval(&source.site, &route.selected, 4096)?;
        let region = source.compile_scalar(interval, true)?;
        if region.calls.len() != 1
            || region.calls[0].callee != native.specialization.resource
            || region
                .results
                .iter()
                .filter(|result| matches!(result.origin, Origin::Computed))
                .count()
                != 1
            || region.survivors.len() != 1
        {
            return Err(Refused::Source);
        }
        let parameters = source
            .params
            .iter()
            .map(|name| {
                binding_slot(
                    source,
                    &Owner::Binding {
                        declared: source.site.clone(),
                        name: name.clone(),
                    },
                )
            })
            .collect::<Result<Box<[_]>, _>>()?;
        Ok(Self {
            library,
            code,
            primitive,
            region,
            parameters,
        })
    }
    pub(crate) fn primitive(&self) -> LibId {
        self.primitive
    }

    pub(crate) fn bind(
        &self,
        exe: &Executable,
        ledger: &Ledger,
        arena: &Arena,
        caller: &logical::Invocation,
        arguments: &[Value],
        result: Value,
    ) -> Result<Option<TextBodyCall>, doc::heap::HeapFault> {
        let function = exe.lib(self.library);
        if !matches!(function.run, LibRun::Body(code) if code == self.code)
            || !Arc::ptr_eq(&self.region.source, &exe.code(self.code).source_room)
            || arguments.len() != 2
            || !function
                .header
                .signature
                .params
                .iter()
                .zip(arguments)
                .all(|(param, value)| compile::import_result_fits(&param.ty, *value))
            || !compile::import_result_fits(&function.header.signature.result, result)
        {
            return Ok(None);
        }
        let [left, right] = arguments else {
            return Ok(None);
        };
        let Some((a, b, actual)) = text(arena, left)
            .zip(text(arena, right))
            .zip(text(arena, &result))
            .map(|((a, b), actual)| (a, b, actual))
        else {
            return Ok(None);
        };
        if a.len().checked_add(b.len()) != Some(actual.len())
            || !actual.as_bytes().starts_with(a.as_bytes())
            || &actual.as_bytes()[a.len()..] != b.as_bytes()
        {
            return Ok(None);
        }
        let primitive = exe.lib(self.primitive);
        let LibRun::Native { native, .. } = &primitive.run else {
            return Ok(None);
        };
        let contract = match native.resource_contract(doc::Args::new(
            arguments,
            arena.objects(),
            &arena.semantics(),
        )) {
            Ok(Some(contract))
                if contract.exit == doc::NativeResourceExit::Returned
                    && contract
                        .events
                        .iter()
                        .all(|event| matches!(event, doc::NativeResourceEvent::Scratch { .. })) =>
            {
                contract
            }
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => {
                return Err(doc::heap::HeapFault::Malformed(reason.into()));
            }
            _ => return Ok(None),
        };
        let Some(native_room) = CheckedRoom::native(primitive.specialization.function, &contract)
        else {
            return Ok(None);
        };
        let mut values = vec![Value::Null; self.region.facts];
        let mut prices = vec![0_i128; self.region.prices];
        let mut given = 0_i128;
        for (slot, value) in self.parameters.iter().zip(arguments) {
            let Some(entry) = self.region.entries.iter().find(|entry| entry.slot == *slot) else {
                return Ok(None);
            };
            values[entry.live.fact.index()] = *value;
            let price = i128::from(arena.logical_local(value)?);
            prices[entry.live.price] = price;
            given += price;
        }
        for fact in &self.region.results {
            let value = match fact.origin {
                Origin::Read(input) => values[input.fact.index()],
                Origin::Computed => result,
                Origin::Literal(_) => return Ok(None),
            };
            values[fact.live.fact.index()] = value;
            prices[fact.live.price] = i128::from(arena.logical_local(&value)?);
        }
        let call = &self.region.calls[0];
        if native_room.balance.retained_delta != prices[call.result.price] {
            return Ok(None);
        }
        prices[call.price] = i128::from(native_room.balance.peak_extra);
        let Some(peak) = self
            .region
            .peaks
            .iter()
            .try_fold(0_i128, |peak, expr| Some(peak.max(expr.evaluate(&prices)?)))
            .and_then(|peak| u64::try_from(peak).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = self.region.delta.evaluate(&prices) else {
            return Ok(None);
        };
        if delta != i128::from(arena.logical_local(&result)?) - given {
            return Ok(None);
        }
        let logical::Owner::Frame { task, .. } = caller.frame() else {
            return Ok(None);
        };
        let Some(reservation) = ledger.reserve_invocations(task, 1) else {
            return Ok(None);
        };
        let handles: Vec<_> = arguments
            .iter()
            .chain(std::iter::once(&result))
            .filter_map(Value::handle)
            .collect();
        let Some(dependencies) = logical::Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        Ok(Some(TextBodyCall {
            room: CheckedRoom {
                owner: self.region.source.owner,
                native: false,
                whole: true,
                native_contract: None,
                balance: Balance {
                    peak_extra: peak,
                    retained_delta: delta,
                },
                exit: ExitKind::Return,
            },
            dependencies,
            reservation,
            caller: caller.clone(),
            depth: 1 + u64::from(primitive.operation()),
        }))
    }
}
impl TextBodyCall {
    pub(crate) fn room(self) -> (CheckedRoom, TextBodyCompletion) {
        (
            self.room,
            TextBodyCompletion {
                dependencies: self.dependencies,
                reservation: self.reservation,
                caller: self.caller,
                depth: self.depth,
            },
        )
    }
}
pub(crate) struct TextBodyCompletion {
    dependencies: logical::Dependencies,
    reservation: logical::InvocationRange,
    caller: logical::Invocation,
    depth: u64,
}
impl TextBodyCompletion {
    pub(crate) fn depth(&self) -> u64 {
        self.depth
    }
    pub(crate) fn finish<'a>(
        self,
        ledger: &Ledger,
        heap: &PagedHeap,
        transaction: logical::BoundaryTransaction<'a>,
    ) -> Result<logical::BoundaryTransaction<'a>, doc::heap::HeapFault> {
        if !self.reservation.is_current(ledger, &self.caller) {
            return Err(doc::heap::HeapFault::Malformed(
                "text body invocation changed".into(),
            ));
        }
        transaction
            .bind_dependencies(self.dependencies)?
            .complete_invocations(self.reservation, 1, heap.pages())
    }
}
fn text<'a>(arena: &'a Arena, value: &'a Value) -> Option<&'a str> {
    match value {
        Value::Text(value) => Some(value.as_str()),
        Value::LongText(handle) => match arena.body(*handle).ok()? {
            doc::arena::Body::LongText(body) => match &*body.chunks {
                [doc::arena::Chunk::Resident(value)] => Some(value),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn binding_slot(source: &Source, owner: &Owner) -> Result<Slot, Refused> {
    let Some(logical::Owner::Value { slot, .. }) =
        source.owner_slots()?.root(owner, TaskId::MAIN, 0)
    else {
        return Err(Refused::Source);
    };
    Ok(slot)
}
