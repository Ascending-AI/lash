//! Regex laws use the actual paged heap and allocation census.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Datum, ErrorDatum, Value};
use lash_kernel_state::paged::census_impl::Census;
use lash_kernel_state::paged::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::rc::Rc;

#[derive(Default)]
struct Source(Rc<RefCell<BTreeMap<PageId, PageAnswer>>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|p| {
                self.0
                    .borrow()
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut data = body.kind.0.to_be_bytes().to_vec();
        data.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&data)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
pub(super) fn fresh() -> PagedHeap {
    let mut heap = PagedHeap::fresh(Box::<Source>::default(), EditEpoch(1));
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(1 << 30)).unwrap(),
    ))
    .unwrap();
    heap
}

fn text(heap: &dyn NativeHeap, value: &TextRef) -> String {
    let mut out = String::new();
    heap.payload_visit(
        &Value::Text(value.clone()),
        0..value.summary.utf8_bytes,
        &mut |chunk| {
            let Chunk::Text(chunk) = chunk else {
                unreachable!()
            };
            out.push_str(chunk);
            ControlFlow::Continue(())
        },
    )
    .unwrap();
    out
}
pub(super) trait Fixture {
    fn record(&mut self, fields: &[(&str, Value)]) -> Value;
    fn regex(&mut self, pattern: &str, flags: &str, last_index: i64) -> Value;
    fn datum(&self, value: &Value) -> Datum;
    fn size(&self, value: &Value) -> u64;
    fn deep_size(&self, value: &Value, seen: &mut BTreeSet<EntityId>) -> u64;
}
impl Fixture for PagedHeap {
    fn record(&mut self, fields: &[(&str, Value)]) -> Value {
        let fields = fields
            .iter()
            .map(|(name, value)| (self.text(name).unwrap(), value.clone()))
            .collect();
        Value::Record(self.allocate(Object::Record(fields)).unwrap())
    }
    fn regex(&mut self, pattern: &str, flags: &str, last_index: i64) -> Value {
        self.record(&[
            ("brand", Value::text(crate::BRAND)),
            ("pattern", Value::text(pattern)),
            ("flags", Value::text(flags)),
            ("lastIndex", Value::Int(last_index.into())),
        ])
    }
    fn datum(&self, value: &Value) -> Datum {
        match value {
            Value::Null => Datum::Null,
            Value::Absent => Datum::Absent,
            Value::Bool(v) => Datum::Bool(*v),
            Value::Int(IntRef::Inline(v)) => Datum::Int(v.clone()),
            Value::Text(v) => Datum::Text(text(self, v)),
            Value::Error(v) => Datum::Error(Box::new(ErrorDatum {
                kind: text(self, &v.kind),
                message: text(self, &v.message),
                data: self.datum(&self.error_data(v).unwrap()),
            })),
            Value::List(id) => {
                let mut out = Vec::new();
                self.visit(*id, &mut |element| {
                    let Element::Item(value) = element else {
                        unreachable!()
                    };
                    out.push(self.datum(value));
                    ControlFlow::Continue(())
                })
                .unwrap();
                Datum::List(out)
            }
            Value::Record(id) => {
                let mut out = Vec::new();
                self.visit(*id, &mut |element| {
                    let Element::Field { name, value } = element else {
                        unreachable!()
                    };
                    out.push((text(self, name), self.datum(value)));
                    ControlFlow::Continue(())
                })
                .unwrap();
                Datum::Record(out)
            }
            _ => panic!("regex fixture has no projection for {value:?}"),
        }
    }
    fn size(&self, value: &Value) -> u64 {
        match value {
            Value::Int(IntRef::Inline(i)) => 1 + i.bits().div_ceil(64),
            Value::Int(IntRef::Paged { summary, .. }) => 1 + summary.bits.div_ceil(64),
            Value::Text(t) => 1 + t.summary.utf8_bytes,
            Value::List(id) | Value::Record(id) => 1 + self.len(*id).unwrap(),
            _ => 1,
        }
    }
    fn deep_size(&self, value: &Value, seen: &mut BTreeSet<EntityId>) -> u64 {
        let Some(id) = value.object() else {
            return self.size(value);
        };
        if !seen.insert(id) {
            return 0;
        }
        let mut total = self.size(value);
        self.visit(id, &mut |element| {
            let (Element::Item(held) | Element::Field { value: held, .. }) = element else {
                unreachable!()
            };
            total += self.deep_size(held, seen);
            ControlFlow::Continue(())
        })
        .unwrap();
        total
    }
}
