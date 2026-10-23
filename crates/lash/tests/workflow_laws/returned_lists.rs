//! FIG-6822: a list a workflow builds through a list method or a
//! comprehension is effect data, as the same list built in a `for` loop is:
//! returned, the process finishes with it. A TypeScript result is what
//! `JSON.stringify` carries, so a member that reads `undefined` is left out
//! and an `undefined` element is `null`.

use super::failures::Module;
use super::*;
use lash::workflow::document::SourceLanguage;

const TYPESCRIPT: &str = "export async function looped(items: number[]): Promise<number[]> {\n  const out = [];\n  for (const item of items) { out.push(item * 2); }\n  return out;\n}\n\nexport async function mapped(items: number[]): Promise<number[]> {\n  return items.map((item) => item * 2);\n}\n\nexport async function mappedRecords(items: number[]): Promise<{ n: number }[]> {\n  return items.map((item) => ({ n: item }));\n}\n\nexport async function filtered(items: number[]): Promise<number[]> {\n  return items.filter((item) => item % 2 === 0);\n}\n\nexport async function sliced(items: number[]): Promise<number[]> {\n  return items.slice(1);\n}\n\nexport async function joined(items: number[]): Promise<number[]> {\n  return items.concat([9]);\n}\n\nexport async function entries(record: { a: number; b: number }): Promise<(string | number)[][]> {\n  return Object.entries(record);\n}\n\nexport async function values(record: { a: number; b: number }): Promise<number[]> {\n  return Object.values(record);\n}\n\nexport async function loopedHits(hits: { id: string; read_ref?: string | undefined }[]): Promise<{ hits: { id: string; read_ref?: string | undefined }[] }> {\n  const out = [];\n  for (const hit of hits) { out.push({ id: hit.id, read_ref: hit.read_ref }); }\n  return { hits: out };\n}\n\nexport async function mappedHits(hits: { id: string; read_ref?: string | undefined }[]): Promise<{ hits: { id: string; read_ref?: string | undefined }[] }> {\n  return { hits: hits.map((hit) => ({ id: hit.id, read_ref: hit.read_ref })) };\n}\n\nexport async function mappedNothing(hits: { id: string; read_ref?: string | undefined }[]): Promise<undefined[]> {\n  return hits.map((hit) => { hit.id; });\n}\n";
const PYTHON: &str = "async def looped(items: list[int]) -> list[int]:\n    out = []\n    for item in items:\n        out.append(item * 2)\n    return out\n\n\nasync def mapped(items: list[int]) -> list[int]:\n    return [item * 2 for item in items]\n\n\nasync def mappedRecords(items: list[int]) -> list[dict[str, int]]:\n    return [{\"n\": item} for item in items]\n\n\nasync def filtered(items: list[int]) -> list[int]:\n    return [item for item in items if item % 2 == 0]\n\n\nasync def sliced(items: list[int]) -> list[int]:\n    return items[1:]\n\n\nasync def joined(items: list[int]) -> list[int]:\n    return items + [9]\n\n\nasync def entries(record: dict[str, int]) -> list[tuple[str, int]]:\n    return list(record.items())\n\n\nasync def values(record: dict[str, int]) -> list[int]:\n    return list(record.values())\n";

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

/// ENTRY-RECORD-DROP: entry records admit evolving input shapes but the body
/// receives only declared fields, recursively through declared containers.
#[tokio::test]
async fn entry_records_drop_undeclared_fields_before_the_body_runs() {
    let core = core().await;
    for (language, source) in [
        (
            SourceLanguage::TypeScript,
            "interface Item { name: string; note?: string }\nexport async function take(event: { items: Item[] }): Promise<JsonValue> { return event; }",
        ),
        (
            SourceLanguage::Python,
            "class Item(TypedDict):\n    name: str\n    note: NotRequired[str]\nclass Event(TypedDict):\n    items: list[Item]\nasync def take(event: Event) -> Any:\n    return event\n",
        ),
    ] {
        let module = Module::import(&core, language, source).await;
        let (_, process) = module
            .start(
                &core,
                "take",
                &[],
                serde_json::json!({
                    "event": {"items": [{"name": "Ada", "extra": 1}], "extra": true}
                }),
            )
            .await;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            core.processes().await_output(&process),
        )
        .await
        .expect("the process settles")
        .expect("the output reads");
        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output
        else {
            panic!("the process settles: {output:?}");
        };
        assert!(output.is_success(), "{language:?}: {output:?}");
        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({"items": [{"name": "Ada"}]})
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// ENTRY-RECORD-CLOSED: explicit closure refuses an extra field at its escaped
/// value path before executing the entry body.
#[tokio::test]
async fn explicitly_closed_entry_records_refuse_the_extra_fields_path() {
    let core = core().await;
    for (language, source) in [
        (
            SourceLanguage::TypeScript,
            "export function take(event: { name: string; [key: string]: never }): JsonValue { throw 'body ran'; }",
        ),
        (
            SourceLanguage::Python,
            "class Event(TypedDict, closed=True):\n    name: str\ndef take(event: Event) -> Any:\n    raise Exception('body ran')\n",
        ),
    ] {
        let module = Module::import(&core, language, source).await;
        let (_, process) = module
            .start(
                &core,
                "take",
                &[],
                serde_json::json!({
                    "event": {"name": "Ada", "a/b~c": true}
                }),
            )
            .await;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            core.processes().await_output(&process),
        )
        .await
        .expect("the process settles")
        .expect("the output reads");
        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output
        else {
            panic!("the process settles: {output:?}");
        };
        let lash::tools::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("closed record refuses");
        };
        let Some(lash::tools::ToolFailureCause::EntryTypeMismatch { source }) =
            failure.cause.as_deref()
        else {
            panic!("typed boundary refusal: {failure:?}");
        };
        assert_eq!(source.mismatch.path, "/input/event/a~1b~0c");
        assert_eq!(source.mismatch.expected, lash::schema::Type::Absent);
        assert_eq!(source.mismatch.actual, lash::schema::Type::Bool);
    }
    core.shutdown().await.expect("shutdown");
}

/// ENTRY-RECORD-KEEP: an explicitly typed JSON rest reaches the entry body.
#[tokio::test]
async fn explicitly_open_entry_records_keep_the_extra_fields() {
    let core = core().await;
    for (language, source) in [
        (
            SourceLanguage::TypeScript,
            "export function take(event: { name: string; [key: string]: JsonValue }): JsonValue { return event; }",
        ),
        (
            SourceLanguage::Python,
            "class Event(TypedDict, extra_items=JsonValue):\n    name: str\ndef take(event: Event) -> Any:\n    return event\n",
        ),
    ] {
        let module = Module::import(&core, language, source).await;
        let event = serde_json::json!({"name": "Ada", "extra": [1, {"ok": true}]});
        let (_, process) = module
            .start(&core, "take", &[], serde_json::json!({"event": event}))
            .await;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            core.processes().await_output(&process),
        )
        .await
        .expect("the process settles")
        .expect("the output reads");
        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output
        else {
            panic!("the process settles: {output:?}");
        };
        assert!(output.is_success(), "{language:?}: {output:?}");
        assert_eq!(output.value_for_projection(), event);
    }
    core.shutdown().await.expect("shutdown");
}
