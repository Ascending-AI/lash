trait IteratorRootView {
    fn restore_value(&self) -> Option<&Value>;
    fn cursor_values(&self) -> Option<&[Value]>;
}

impl IteratorRootView for IterState {
    fn restore_value(&self) -> Option<&Value> {
        self.restore.previous.as_ref()
    }

    fn cursor_values(&self) -> Option<&[Value]> {
        match &self.cursor {
            IterCursor::List { values, .. } => Some(values),
            IterCursor::Range { .. } => None,
        }
    }
}

impl IteratorRootView for VmIteratorContinuation {
    fn restore_value(&self) -> Option<&Value> {
        self.restore_value.as_ref()
    }

    fn cursor_values(&self) -> Option<&[Value]> {
        match &self.cursor {
            VmIteratorCursor::List { values, .. } => Some(values),
            VmIteratorCursor::Range { .. } => None,
        }
    }
}

trait FrameRootView {
    type Iterator: IteratorRootView;

    fn caller_is_root(&self) -> bool;
    fn slots(&self) -> &[Option<Value>];
    fn globals(&self) -> &Record;
    fn iterators(&self) -> &[Self::Iterator];
    fn map_roots(&self) -> Option<(&Value, &[Value], &[Value])>;
}

trait FinallyRootView {
    fn thrown_value(&self) -> Option<&Value>;
}

impl FinallyRootView for FinallyState {
    fn thrown_value(&self) -> Option<&Value> {
        match &self.completion {
            FinallyCompletion::Throw { value, .. } => Some(value),
            FinallyCompletion::Normal { .. } => None,
        }
    }
}

impl FinallyRootView for VmFinallyContinuation {
    fn thrown_value(&self) -> Option<&Value> {
        match &self.completion {
            VmFinallyCompletionContinuation::Throw { value, .. } => Some(value),
            VmFinallyCompletionContinuation::Normal { .. } => None,
        }
    }
}

impl FrameRootView for CallFrame {
    type Iterator = IterState;

    fn caller_is_root(&self) -> bool {
        self.function.is_none()
    }

    fn slots(&self) -> &[Option<Value>] {
        &self.slots.values
    }

    fn globals(&self) -> &Record {
        &self.slots.extras
    }

    fn iterators(&self) -> &[Self::Iterator] {
        &self.iter_stack
    }

    fn map_roots(&self) -> Option<(&Value, &[Value], &[Value])> {
        match &self.return_target {
            ReturnTarget::Direct => None,
            ReturnTarget::Map(callback) => {
                Some((&callback.function, &callback.items, &callback.results))
            }
        }
    }
}

impl FrameRootView for VmFrameContinuation {
    type Iterator = VmIteratorContinuation;

    fn caller_is_root(&self) -> bool {
        self.function.is_none()
    }

    fn slots(&self) -> &[Option<Value>] {
        &self.slots
    }

    fn globals(&self) -> &Record {
        &self.globals
    }

    fn iterators(&self) -> &[Self::Iterator] {
        &self.iterator_stack
    }

    fn map_roots(&self) -> Option<(&Value, &[Value], &[Value])> {
        match &self.return_target {
            VmFrameReturnContinuation::Direct => None,
            VmFrameReturnContinuation::Map {
                function,
                items,
                results,
                ..
            } => Some((function, items, results)),
        }
    }
}

trait VmRootView {
    type Iterator: IteratorRootView;
    type Frame: FrameRootView;
    type Finally: FinallyRootView;

    fn has_active_function(&self) -> bool;
    fn slots(&self) -> &[Option<Value>];
    fn globals(&self) -> &Record;
    fn operand_stack(&self) -> &[Value];
    fn last_value(&self) -> Option<&Value>;
    fn iterators(&self) -> &[Self::Iterator];
    fn frames(&self) -> &[Self::Frame];
    fn finalizers(&self) -> &[Self::Finally];
}

impl<H> VmRootView for Vm<'_, H> {
    type Iterator = IterState;
    type Frame = CallFrame;
    type Finally = FinallyState;

    fn has_active_function(&self) -> bool {
        self.active_function.is_some()
    }

    fn slots(&self) -> &[Option<Value>] {
        &self.slots.values
    }

    fn globals(&self) -> &Record {
        &self.slots.extras
    }

    fn operand_stack(&self) -> &[Value] {
        &self.stack
    }

    fn last_value(&self) -> Option<&Value> {
        self.last_value.as_ref()
    }

    fn iterators(&self) -> &[Self::Iterator] {
        &self.iter_stack
    }

    fn frames(&self) -> &[Self::Frame] {
        &self.frames
    }

    fn finalizers(&self) -> &[Self::Finally] {
        &self.finally_stack
    }
}

impl VmRootView for VmContinuation {
    type Iterator = VmIteratorContinuation;
    type Frame = VmFrameContinuation;
    type Finally = VmFinallyContinuation;

    fn has_active_function(&self) -> bool {
        self.active_function.is_some()
    }

    fn slots(&self) -> &[Option<Value>] {
        &self.slots
    }

    fn globals(&self) -> &Record {
        &self.globals
    }

    fn operand_stack(&self) -> &[Value] {
        &self.operand_stack
    }

    fn last_value(&self) -> Option<&Value> {
        self.last_value.as_ref()
    }

    fn iterators(&self) -> &[Self::Iterator] {
        &self.iterator_stack
    }

    fn frames(&self) -> &[Self::Frame] {
        &self.frame_stack
    }

    fn finalizers(&self) -> &[Self::Finally] {
        &self.finally_stack
    }
}

/// Enumerates every heap root held by active execution or a parked caller.
///
/// Durable roots own their objects for ADR-0059 forest validation. Once a call
/// is active, the saved root frame remains the durable owner while active and
/// saved function frames are transient borrowers. This deliberately permits a
/// named function's `self_slot` to alias the closure still owned by its caller.
/// GC uses the same walk but treats both classes as reachability roots.
trait VmRootVisitor<'a> {
    fn durable(&mut self, name: String, value: &'a Value);
    fn transient(&mut self, value: &'a Value);
}

impl<'a> VmRootVisitor<'a> for Vec<Value> {
    fn durable(&mut self, _name: String, value: &'a Value) {
        self.push(value.clone());
    }

    fn transient(&mut self, value: &'a Value) {
        self.push(value.clone());
    }
}

impl<'a> VmRootVisitor<'a> for PersistedRoots<'a> {
    fn durable(&mut self, name: String, value: &'a Value) {
        PersistedRoots::durable(self, name, value);
    }

    fn transient(&mut self, value: &'a Value) {
        PersistedRoots::transient(self, value);
    }
}

fn visit_vm_roots<'a, V: VmRootView>(view: &'a V, visitor: &mut impl VmRootVisitor<'a>) {
    if view.has_active_function() {
        if let Some(root_frame) = view.frames().iter().find(|frame| frame.caller_is_root()) {
            for (index, value) in root_frame.slots().iter().enumerate() {
                if let Some(value) = value {
                    visitor.durable(format!("root frame slot {index}"), value);
                }
            }
            for (name, value) in root_frame.globals().iter() {
                visitor.durable(format!("root frame global `{name}`"), value);
            }
        }
        for value in view.slots().iter().flatten() {
            visitor.transient(value);
        }
        for value in view.globals().values() {
            visitor.transient(value);
        }
    } else {
        for (index, value) in view.slots().iter().enumerate() {
            if let Some(value) = value {
                visitor.durable(format!("slot {index}"), value);
            }
        }
        for (name, value) in view.globals().iter() {
            visitor.durable(format!("global `{name}`"), value);
        }
    }

    for value in view.operand_stack() {
        visitor.transient(value);
    }
    if let Some(value) = view.last_value() {
        visitor.transient(value);
    }
    for finally in view.finalizers() {
        if let Some(value) = finally.thrown_value() {
            visitor.transient(value);
        }
    }
    for (depth, iterator) in view.iterators().iter().enumerate() {
        if let Some(value) = iterator.restore_value() {
            visitor.durable(format!("iterator {depth} restore value"), value);
        }
        if let Some(values) = iterator.cursor_values() {
            for value in values {
                visitor.transient(value);
            }
        }
    }

    for frame in view.frames() {
        for value in frame.slots().iter().flatten() {
            visitor.transient(value);
        }
        for value in frame.globals().values() {
            visitor.transient(value);
        }
        for iterator in frame.iterators() {
            if let Some(value) = iterator.restore_value() {
                visitor.transient(value);
            }
            if let Some(values) = iterator.cursor_values() {
                for value in values {
                    visitor.transient(value);
                }
            }
        }
        if let Some((function, items, results)) = frame.map_roots() {
            visitor.transient(function);
            for value in items {
                visitor.transient(value);
            }
            for value in results {
                visitor.transient(value);
            }
        }
    }
}
