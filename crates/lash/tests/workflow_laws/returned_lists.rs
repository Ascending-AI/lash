//! FIG-6822: a list a workflow builds through a list method or a
//! comprehension is effect data, as the same list built in a `for` loop is:
//! returned, the process finishes with it. A TypeScript result is what
//! `JSON.stringify` carries, so a member that reads `undefined` is left out
//! and an `undefined` element is `null`.

use super::failures::Module;
use super::*;
use lash::workflow::document::SourceLanguage;

const TYPESCRIPT: &str = "export async function looped(items: number[]) {\n  const out = [];\n  for (const item of items) { out.push(item * 2); }\n  return out;\n}\n\nexport async function mapped(items: number[]) {\n  return items.map((item) => item * 2);\n}\n\nexport async function mappedRecords(items: number[]) {\n  return items.map((item) => ({ n: item }));\n}\n\nexport async function filtered(items: number[]) {\n  return items.filter((item) => item % 2 === 0);\n}\n\nexport async function sliced(items: number[]) {\n  return items.slice(1);\n}\n\nexport async function joined(items: number[]) {\n  return items.concat([9]);\n}\n\nexport async function entries(record: any) {\n  return Object.entries(record);\n}\n\nexport async function values(record: any) {\n  return Object.values(record);\n}\n\nexport async function loopedHits(hits: any) {\n  const out = [];\n  for (const hit of hits) { out.push({ id: hit.id, read_ref: hit.read_ref }); }\n  return { hits: out };\n}\n\nexport async function mappedHits(hits: any) {\n  return { hits: hits.map((hit) => ({ id: hit.id, read_ref: hit.read_ref })) };\n}\n\nexport async function mappedNothing(hits: any) {\n  return hits.map((hit) => { hit.id; });\n}\n";
const PYTHON: &str = "async def looped(items):\n    out = []\n    for item in items:\n        out.append(item * 2)\n    return out\n\n\nasync def mapped(items):\n    return [item * 2 for item in items]\n\n\nasync def mappedRecords(items):\n    return [{\"n\": item} for item in items]\n\n\nasync def filtered(items):\n    return [item for item in items if item % 2 == 0]\n\n\nasync def sliced(items):\n    return items[1:]\n\n\nasync def joined(items):\n    return items + [9]\n\n\nasync def entries(record: dict):\n    return list(record.items())\n\n\nasync def values(record: dict):\n    return list(record.values())\n";

/// Each entry returns a list derived from its argument, in both dialects;
/// each process finishes with that list as its value.
#[tokio::test]
async fn a_list_from_a_list_method_or_comprehension_is_a_workflows_result() {
    let core = core().await;
    let items = serde_json::json!({"items": [1, 2, 3, 4]});
    let record = serde_json::json!({"record": {"a": 1, "b": 2}});
    let hits = serde_json::json!({"hits": [{"id": "a", "read_ref": "r1"}, {"id": "b"}]});
    let both = [
        ("looped", &items, serde_json::json!([2, 4, 6, 8])),
        ("mapped", &items, serde_json::json!([2, 4, 6, 8])),
        (
            "mappedRecords",
            &items,
            serde_json::json!([{"n": 1}, {"n": 2}, {"n": 3}, {"n": 4}]),
        ),
        ("filtered", &items, serde_json::json!([2, 4])),
        ("sliced", &items, serde_json::json!([2, 3, 4])),
        ("joined", &items, serde_json::json!([1, 2, 3, 4, 9])),
        ("entries", &record, serde_json::json!([["a", 1], ["b", 2]])),
        ("values", &record, serde_json::json!([1, 2])),
    ];
    let undefined = [
        (
            "loopedHits",
            &hits,
            serde_json::json!({"hits": [{"id": "a", "read_ref": "r1"}, {"id": "b"}]}),
        ),
        (
            "mappedHits",
            &hits,
            serde_json::json!({"hits": [{"id": "a", "read_ref": "r1"}, {"id": "b"}]}),
        ),
        ("mappedNothing", &hits, serde_json::json!([null, null])),
    ];
    for (language, source, cases) in [
        (
            SourceLanguage::TypeScript,
            TYPESCRIPT,
            both.iter().chain(&undefined).collect::<Vec<_>>(),
        ),
        (SourceLanguage::Python, PYTHON, both.iter().collect()),
    ] {
        let module = Module::import(&core, language, source).await;
        for (entry, args, expected) in cases {
            let (_, process) = module.start(&core, entry, &[], (*args).clone()).await;
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                core.processes().await_output(&process),
            )
            .await
            .expect("the process settles")
            .expect("the output reads");
            let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output
            else {
                panic!("{language:?} {entry}: the process settles with an output: {output:?}");
            };
            assert!(
                output.is_success(),
                "{language:?} {entry}: the process finishes: {output:?}"
            );
            assert_eq!(
                &output.value_for_projection(),
                expected,
                "{language:?} {entry}"
            );
        }
    }
    core.shutdown().await.expect("shutdown");
}
