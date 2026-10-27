//! Bulk copies of a certified body, including its ordinary closure charges.

use super::{Args, HeapCx, NewObject, Value};

impl HeapCx<'_, '_> {
    pub(super) fn concat_sources(
        &mut self,
        receiver: Value,
        arguments: Value,
        read_units: u64,
    ) -> Option<Value> {
        self.copy_sources(receiver, Some(arguments), read_units, None)
    }

    pub(super) fn dense_source(
        &mut self,
        source: Value,
        read_units: u64,
        loop_units: u64,
    ) -> Option<Value> {
        if !matches!(source, Value::List(_)) {
            return None;
        }
        self.copy_sources(source, None, read_units, Some(loop_units))
    }

    fn copy_sources(
        &mut self,
        receiver: Value,
        arguments: Option<Value>,
        read_units: u64,
        loop_units: Option<u64>,
    ) -> Option<Value> {
        if !self.cx.exe.charging()
            || self.cx.meter.recording()
            || self.cx.native_observer.is_some()
            || self.cx.held.is_some()
            || self.cx.concatenated()
            || self.cx.arena.any_cold()
            || matches!(receiver, Value::Null | Value::Absent | Value::Record(_))
            || arguments.is_some_and(|args| !matches!(args, Value::List(_)))
        {
            return None;
        }
        let (values, reads, nested) = {
            let semantics = self.cx.arena.semantics().with_limits(self.cx.limits);
            let args = Args::new(&[], self.cx.arena.objects(), &semantics);
            let sources = match arguments {
                Some(value) => Some(args.sequence(&value).ok()??),
                None => None,
            };
            let length = |source: &Value| -> Option<u64> {
                match source {
                    Value::List(_) => {
                        let length = args.sequence(source).ok()??.len();
                        (length <= lash_kernel_doc::plan::ELEMENT_POSITIONS).then_some(length)
                    }
                    Value::Record(_) => None,
                    _ => Some(1),
                }
            };
            let mut count = length(&receiver)?;
            for chunk in sources.into_iter().flat_map(|source| source.chunks()) {
                for source in chunk.ok()? {
                    count = count.checked_add(length(source)?)?;
                }
            }
            // The entry separately admits the source call frames. Leave
            // room for its temporary source list and closure holdings too;
            // tight headroom retains the body's first refusal and prefix.
            let room = count
                .checked_add(sources.map_or(0, |source| source.len()))?
                .checked_add(1)?
                .checked_mul(lash_kernel_state::arena::VALUE_BYTES)?
                .checked_add(1024)?;
            if self.cx.memory
                && room
                    > self
                        .cx
                        .copy_ceiling
                        .saturating_sub(self.cx.arena.held())
                        .saturating_sub(self.cx.transient)
            {
                return None;
            }
            let mut values = Vec::new();
            values
                .try_reserve_exact(usize::try_from(count).ok()?)
                .ok()?;
            let mut reads = 0_u64;
            let mut nested = 0_u64;
            let mut append = |source: &Value| -> Option<()> {
                if matches!(source, Value::List(_)) {
                    for chunk in args.sequence(source).ok()??.chunks() {
                        let chunk = chunk.ok()?;
                        let start = values.len();
                        values.extend_from_slice(chunk);
                        reads = reads.checked_add(chunk.len() as u64)?;
                        for (offset, value) in chunk.iter().enumerate() {
                            match value {
                                Value::Tuple(_) if args.sequence(value).ok()??.is_empty() => {
                                    if loop_units.is_some() {
                                        values[start + offset] = Value::Absent;
                                    } else {
                                        reads = reads.checked_sub(1)?;
                                    }
                                }
                                Value::Tuple(_) | Value::Error(_) => {
                                    if self.cx.arena.summary_of(value).depth as usize
                                        > self.cx.limits.value_depth()
                                    {
                                        return None;
                                    }
                                    nested = nested.checked_add(
                                        crate::data::nested_size(self.cx.arena, value).ok()?,
                                    )?;
                                }
                                // Other values have zero immutable depth
                                // and nested comparison size.
                                _ => {}
                            }
                        }
                    }
                } else {
                    values.push(*source);
                }
                Some(())
            };
            append(&receiver)?;
            for chunk in sources.into_iter().flat_map(|source| source.chunks()) {
                for source in chunk.ok()? {
                    append(source)?;
                }
            }
            (values, reads, nested)
        };
        let units = read_units.checked_mul(reads)?.checked_add(nested)?;
        let units = units.checked_add(match loop_units {
            Some(units) => units.checked_mul((values.len() as u64).checked_add(1)?)?,
            None => 0,
        })?;
        let ceiling = self.cx.ceiling;
        self.cx.ceiling = self.cx.copy_ceiling;
        let value = self.grown(|paths| paths.cx.build(NewObject::List(values)));
        self.cx.ceiling = ceiling;
        // Other heap operations retain their ordinary allowance. Another
        // known-size copy checks the remaining admitted memory again.
        let value = value?;
        self.built = true;
        // Reads enter an ordinary closure, even inside an uncharged library
        // body. Its charges and declared loop tests survive that boundary.
        self.charges.push((units, None, true));
        Some(value)
    }
}
