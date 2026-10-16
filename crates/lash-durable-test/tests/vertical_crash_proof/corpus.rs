//! FIG-6183: every retained parked-run image resumes to its recorded result.
use super::*;

const CURRENT_SET_OUTPUT: &str = "crates/lash-durable-test/tests/fixtures/formats/corpus/4b1b3a0d684e448108de3ac8f13d0a153ffe0ef1";

fn current_set() -> &'static images::ImageSet {
    let build = CURRENT_SET_OUTPUT.rsplit('/').next().unwrap();
    images::retained()
        .iter()
        .find(|set| set.build == build)
        .expect("the current set is retained")
}

/// The land step's literal discovery path must cover the set we refresh.
#[test]
fn generator_output_names_the_current_set() {
    let source = include_str!("corpus.rs");
    let (declaration, _) = source
        .split_once("\nasync fn regenerate_current_parked_image_set()")
        .expect("the current corpus has a generator");
    let output = declaration
        .lines()
        .rev()
        .find_map(|line| {
            line.strip_prefix("#[ignore = \"regenerates ")
                .and_then(|line| line.strip_suffix("\"]"))
        })
        .expect("the generator declares its output");
    assert_eq!(output, CURRENT_SET_OUTPUT);
    assert_eq!(output.rsplit('/').next().unwrap(), current_set().build);
}

const LOOP: &str = r#"kernel 1
numbers float
entry worker() -> Any
fn worker() {
  let values = [0]
  let alias = values
  let count = 0
  let bump = fn() { set count = num.add(count, 1) return count }
  for step in [1, 2, 3] {
    do apply bump()
    set alias[0] = num.add(values[0], step)
    do sleep 5000
  }
  return (values[0], count, alias[0])
}
main { finish null }
"#;
const JOIN: &str = r#"kernel 1
numbers float
entry worker() -> Any
fn child() { do sleep 5000 return "joined child" }
fn worker() { let task = spawn call child() let answer = join task return answer }
main { finish null }
"#;
const CLEANUP: &str = r#"kernel 1
numbers float
entry worker() -> Any
fn worker() {
  let values = ["before"]
  try { return values } finally {
    do sleep 5000
    set values[0] = "cleanup completed"
  }
}
main { finish null }
"#;
const PYTHON_CELL: &str =
    "<python>\nwritten = await tools_ext_write({\"x\": 7})\nprint(written)\n</python>";
const READ_CELL: &str = "<typescript>\nconst read = await tools.ext_read({ x: 7 });\nconst written = await tools.ext_write({ x: read.x });\nconsole.log([read, written]);\n</typescript>";

struct SavedRead {
    world: Arc<ExternalWorld>,
}
#[async_trait::async_trait]
impl lash::tools::ToolReads for SavedRead {
    async fn read(
        &self,
        call: lash::tools::ReadCall<'_>,
    ) -> Result<serde_json::Value, lash::tools::ReadError> {
        *self.world.reads.lock_recover() += 1;
        Ok(call.args.clone())
    }
}

pub(super) fn reader_plugin(world: &Arc<ExternalWorld>) -> Arc<dyn lash::plugins::PluginFactory> {
    let definition = lash_core::ToolDefinition::raw(
        "ext_read", "ext_read", "Reads x without a durable call record.",
        serde_json::json!({"type":"object", "properties":{"x":{"type":"number"}}, "required":["x"], "additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    ).unwrap().with_execution(Duration::from_secs(120))
        .with_execution_policy(ExecutionPolicy::Read)
        .with_tool_binding(lash_core::ToolBinding::new(["tools"], "ext_read"));
    let provider = Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite {
            world: Arc::clone(world),
        },
    ));
    Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("retained-image-reader"),
        lash::plugins::PluginSpec::new().with_tool_provider_registration(
            lash::tool_provider_key!("reads"),
            lash::tools::ToolProviderRegistration::new(provider).with_reads(Arc::new(SavedRead {
                world: Arc::clone(world),
            })),
        ),
    ))
}

fn protocol(image: &'static images::RetainedImage) -> Protocol {
    // The sources belong to the static archive; no source is lowered on resume.
    Protocol::ImageCell(&image.source, image.kind == "python-cell")
}

async fn resume(image: &'static images::RetainedImage) -> serde_json::Value {
    let stored = Box::leak(Box::new(image.image()));
    match image.kind.as_str() {
        "kernel" => kernel_images::resume(stored).await,
        "typescript-cell" | "python-cell" | "declared-read-cell" => {
            session_images::resume(stored, protocol(image)).await
        }
        kind => panic!("{} has unknown image kind {kind}", image.name),
    }
}

/// Parked images are upgrade witnesses, including the original 1.0 stores.
#[tokio::test(flavor = "current_thread")]
async fn every_retained_parked_image_resumes_to_its_recorded_result() {
    let end = kernel_images::resume(&images::LASH_VM).await;
    assert!(end.to_string().contains("\"done\""), "1.0 kernel: {end}");
    session_images::resume(&images::SESSION, Protocol::Code).await;
    assert!(
        !images::retained().is_empty(),
        "record the first realistic image set"
    );
    let mut builds = std::collections::BTreeSet::new();
    for set in images::retained() {
        assert!(builds.insert(&set.build), "duplicate build {}", set.build);
        assert_eq!(set.build.len(), 40, "record the full build commit");
        assert!(
            !set.helper_releases.as_array().unwrap().is_empty(),
            "name the helper releases"
        );
        assert_eq!(
            set.images.len(),
            6,
            "retain all six realistic shapes on {}",
            set.build
        );
        let mut names = std::collections::BTreeSet::new();
        for image in &set.images {
            assert!(names.insert(&image.name), "duplicate image {}", image.name);
            assert_eq!(
                resume(image).await,
                image.expected,
                "{} on {}",
                image.name,
                set.build
            );
        }
    }
}

/// Record and cold-resume the archived program on the current build.
async fn record_image(name: &str, kind: &str, source: &'static str) -> images::RetainedImage {
    match kind {
        "kernel" => {
            let (_dir, path) = kernel_images::record(source).await;
            let mut image = images::capture(&path, name, kind, source);
            image.expected = kernel_images::resume(&image.image()).await;
            image
        }
        "typescript-cell" | "python-cell" | "declared-read-cell" => {
            let protocol = Protocol::ImageCell(source, kind == "python-cell");
            let (_dir, path) = session_images::record(protocol).await;
            let mut image = images::capture(&path, name, kind, source);
            let stored = Box::leak(Box::new(image.image()));
            image.expected = session_images::resume(stored, protocol).await;
            image
        }
        kind => panic!("{name} has unknown image kind {kind}"),
    }
}

/// Refresh the unsealed set without changing its metadata, programs or oracles.
#[tokio::test(flavor = "current_thread")]
#[ignore = "regenerates crates/lash-durable-test/tests/fixtures/formats/corpus/4b1b3a0d684e448108de3ac8f13d0a153ffe0ef1"]
async fn regenerate_current_parked_image_set() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let set = current_set();
    let mut records = Vec::new();
    for old in &set.images {
        let fresh = record_image(&old.name, &old.kind, &old.source).await;
        assert_eq!(
            fresh.expected, old.expected,
            "a refresh keeps {}'s recorded result",
            old.name
        );
        records.push(fresh);
    }
    // Verify the complete set before replacing any payload.
    for image in records {
        images::rerecord(&set.build, image);
    }
}

/// Append a full set at a release cut, or refresh one authorized pre-cut
/// witness selected by LASH_IMAGE_NAME without changing its metadata or oracle.
#[tokio::test(flavor = "current_thread")]
#[ignore = "appends a new retained parked-run image set"]
async fn record_current_parked_image_set() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let build = std::env::var("LASH_IMAGE_BUILD").expect("name the recording build commit");
    let selected = std::env::var("LASH_IMAGE_NAME").ok();
    let mut records = Vec::new();
    for (name, kind, source) in [
        ("loop-closures-aliases", "kernel", LOOP),
        ("task-join", "kernel", JOIN),
        ("suspending-cleanup", "kernel", CLEANUP),
        ("pending-tool-typescript", "typescript-cell", CELL),
        ("pending-tool-python", "python-cell", PYTHON_CELL),
        ("saved-declared-read", "declared-read-cell", READ_CELL),
    ] {
        if selected.as_deref().is_some_and(|selected| selected != name) {
            continue;
        }
        records.push(record_image(name, kind, source).await);
    }
    if selected.is_some() {
        assert_eq!(
            records.len(),
            1,
            "select one of the six retained image names"
        );
        images::rerecord(&build, records.pop().unwrap());
        return;
    }
    images::append(images::ImageSet {
        build,
        kernel_semantics: lash_vm_runtime::LASH_KERNEL_VERSION,
        helper_releases: serde_json::to_value(lash_vm_runtime::standard_helper_releases().unwrap())
            .unwrap(),
        images: records,
    });
}
