//! A canonical prepared text result retains its forwarding body's envelope.
use super::*;
use crate::compile::room::publication::text::PreparedConcat;
impl TextBody {
    pub(crate) fn bind_prepared(
        &self,
        exe: &Executable,
        ledger: &Ledger,
        arena: &Arena,
        caller: &logical::Invocation,
        prepared: &PreparedConcat,
    ) -> Result<Option<TextBodyCall>, doc::heap::HeapFault> {
        let function = exe.lib(self.library);
        if !matches!(function.run, LibRun::Body(code) if code == self.code)
            || !Arc::ptr_eq(&self.region.source, &exe.code(self.code).source_room)
            || prepared.primitive() != self.primitive
        {
            return Ok(None);
        }
        let arguments = prepared.arguments();
        let result = prepared.result();
        let primitive = exe.lib(self.primitive);
        let Some(native_room) =
            CheckedRoom::native(primitive.specialization.function, prepared.contract())
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
