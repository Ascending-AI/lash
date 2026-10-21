//! Regex laws call the natives on an arena bench, as the machine calls them.
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{Datum, NewObject, Value};
pub(super) use lash_kernel_state::arena::Bench;

/// The identity `compile_check` stamps the records it builds with.
pub(super) fn built() -> lash_vocabulary::legacy_names::FunctionId {
    crate::definitions::Operation::CompileCheck
        .definition()
        .identity()
}

pub(super) fn fresh() -> Bench {
    Bench::new()
}

pub(super) trait Fixture {
    fn text(&mut self, text: &str) -> Value;
    fn record(&mut self, fields: &[(&str, Value)]) -> Value;
    fn regex(&mut self, pattern: &str, flags: &str, last_index: i64) -> Value;
    /// What `value` holds, as data.
    fn read(&self, value: &Value) -> Datum;
    fn size(&self, value: &Value) -> u64;
    fn deep_size(&self, value: &Value, seen: &mut Vec<ArenaHandle>) -> u64;
}
impl Fixture for Bench {
    fn text(&mut self, text: &str) -> Value {
        self.arena_mut().text(text)
    }
    fn record(&mut self, fields: &[(&str, Value)]) -> Value {
        let fields: Vec<_> = fields
            .iter()
            .map(|(name, value)| (self.text(name), *value))
            .collect();
        self.run(&[], u64::MAX, |_, new| {
            new.allocate(NewObject::Record(fields))
        })
        .unwrap()
        .0
    }
    fn regex(&mut self, pattern: &str, flags: &str, last_index: i64) -> Value {
        let fields = [
            (
                "__lash_brand",
                self.run(&[], u64::MAX, |_, new| {
                    let package = new.text(crate::BRAND);
                    let ty = new.text("reg_exp");
                    new.allocate(NewObject::Tuple(vec![package, ty]))
                })
                .unwrap()
                .0,
            ),
            (
                "__lash_built",
                self.run(&[], u64::MAX, |_, new| new.bytes(built().as_bytes()))
                    .unwrap()
                    .0,
            ),
            ("pattern", self.text(pattern)),
            ("flags", self.text(flags)),
            ("lastIndex", Value::Int(last_index)),
        ];
        self.record(&fields)
    }
    fn read(&self, value: &Value) -> Datum {
        self.datum(value).unwrap()
    }
    fn size(&self, value: &Value) -> u64 {
        let semantics = self.arena().semantics();
        match value {
            Value::Int(_) | Value::BigInt(_) => {
                1 + semantics
                    .integer(value)
                    .unwrap()
                    .unwrap()
                    .bits()
                    .div_ceil(64)
            }
            Value::Text(_) | Value::LongText(_) => 1 + semantics.text(value).unwrap().len() as u64,
            Value::Bytes(id) => 1 + semantics.bytes(*id).unwrap().len() as u64,
            Value::Tuple(id) | Value::List(id) | Value::Record(id) => {
                1 + self.arena().len(*id).unwrap()
            }
            _ => 1,
        }
    }
    fn deep_size(&self, value: &Value, seen: &mut Vec<ArenaHandle>) -> u64 {
        let semantics = self.arena().semantics();
        let held = match value {
            Value::Tuple(id) | Value::List(id) => semantics.sequence(*id).unwrap().to_vec(),
            Value::Record(id) => semantics
                .entries(*id)
                .unwrap()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| pair[1])
                .collect(),
            _ => return self.size(value),
        };
        let id = value.handle().unwrap();
        if seen.contains(&id) {
            return 0;
        }
        seen.push(id);
        let mut total = self.size(value);
        for held in held {
            total += self.deep_size(&held, seen);
        }
        total
    }
}
