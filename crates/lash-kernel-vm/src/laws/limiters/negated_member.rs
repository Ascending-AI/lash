//! Membership and negation fees on both arms of a traced guard.

use super::super::embedder::Probes;
use super::huge_charges;

const ROW: &str = r#"main {
  let record = {present: 7}
  let total = 0
  let key = "present"
  let i = 0
  while num.lt(i, 2) {
    let target = record
    let field = key
    if law.not(law.member(target, field)) {
      set total = num.add(total, 1)
    } else {
      set total = num.add(total, 10)
    }
    set key = "missing"
    set i = num.add(i, 1)
  }
  return total
}"#;

fn probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
    let mut registered = huge_charges(registry, ids);
    for (name, signature, implementation) in [
        (
            "law.member",
            "law.member(target: Any, key: Any) -> Bool",
            "record.contains",
        ),
        ("law.not", "law.not(value: Any) -> Bool", "bool.not"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {signature}\nkernel 1\ncharge 1\nnative\n"
        ))
        .unwrap();
        let function = registry
            .register(
                definition,
                Some(lash_kernel_lib::native(implementation).unwrap()),
            )
            .unwrap();
        registered.push((name, function));
    }
    registered
}

pub(super) fn rows<const N: usize>(
    ordinary: [&'static str; N],
) -> impl Iterator<Item = (&'static str, Option<Probes>)> {
    ordinary
        .map(|text| (text, None))
        .into_iter()
        .chain([(ROW, Some(probes as Probes))])
}
