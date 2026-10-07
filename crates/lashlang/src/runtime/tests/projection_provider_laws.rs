//! Projection providers (ADR 0132 §9): a projection value is plain data that
//! reads through the provider registered for its type, wherever it is read.

use super::*;
use crate::{ProjectionError, ProjectionProvider, ProjectionReader};

/// A list provider that counts the reads it answers.
#[derive(Default)]
struct Rows {
    reads: AtomicUsize,
}

impl Rows {
    fn rows() -> Vec<Value> {
        vec![
            Value::String("first".into()),
            Value::String("second".into()),
        ]
    }

    fn answer(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        match request {
            ProjectedReadRequest::Len => Some(ProjectedReadResponse::Len(Self::rows().len())),
            ProjectedReadRequest::Index(Value::Number(index)) => Self::rows()
                .get(index as usize)
                .cloned()
                .map(ProjectedReadResponse::Value),
            ProjectedReadRequest::Materialize => Some(ProjectedReadResponse::Value(Value::List(
                Self::rows().into(),
            ))),
            _ => None,
        }
    }
}

#[async_trait::async_trait]
impl ProjectionProvider for Rows {
    fn projection_type(&self) -> ProjectionType {
        ProjectionType::new("rows")
    }

    async fn read(
        &self,
        _resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionError> {
        Ok(self.answer(request))
    }

    async fn read_range(
        &self,
        _resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionError> {
        Ok(requests
            .into_iter()
            .map(|request| self.answer(request))
            .collect())
    }
}

fn rows_value() -> ProjectedValue {
    ProjectedValue::resource(
        "rows",
        "list",
        ResourceRef {
            projection: ProjectionType::new("rows"),
            id: "session-1".into(),
            revision: Some("r1".into()),
        },
    )
}

fn catalog_of(provider: Option<Arc<Rows>>) -> Arc<dyn ProjectionReader> {
    let mut catalog = ProjectionCatalog::new();
    if let Some(provider) = provider {
        catalog
            .register(provider as Arc<dyn ProjectionProvider>)
            .expect("one provider of `rows`");
    }
    Arc::new(crate::testing::projection::CatalogReader(catalog))
}

async fn run(
    program: Program,
    state: &mut State,
    bindings: ProjectedBindings,
) -> Result<Value, RuntimeError> {
    match execute_compiled_with_projected_bindings(
        &compile_program(&program),
        state,
        &Host,
        &bindings,
    )
    .await?
    {
        ExecutionOutcome::Finished(value) => Ok(value),
        other => panic!("expected `finish`, got {other:?}"),
    }
}

/// The program a restored state runs: every read of the kept projections.
fn read_kept() -> Program {
    builders::program(vec![builders::finish(builders::list(vec![
        builders::builtin(
            "len",
            vec![builders::index(builders::var("kept"), builders::num(0.0))],
        ),
        builders::builtin(
            "len",
            vec![builders::field(
                builders::index(builders::var("kept"), builders::num(1.0)),
                "inner",
            )],
        ),
        builders::index(
            builders::index(builders::var("kept"), builders::num(0.0)),
            builders::num(1.0),
        ),
    ]))])
}

/// A state whose heap keeps `rows` in a list and in a record, captured as
/// canonical snapshot bytes.
async fn kept_projections_snapshot() -> Vec<u8> {
    let first = Arc::new(Rows::default());
    let mut bindings = ProjectedBindings::new().with_reader(catalog_of(Some(first.clone())));
    bindings.insert("rows", rows_value());
    let mut state = State::new();
    run(
        builders::program(vec![
            builders::assign(
                "kept",
                builders::list(vec![
                    builders::var("rows"),
                    builders::record(vec![("inner", builders::var("rows"))]),
                ]),
            ),
            builders::finish(builders::num(0.0)),
        ]),
        &mut state,
        bindings,
    )
    .await
    .expect("the first execution keeps the projections");
    assert_eq!(
        first.reads.load(Ordering::SeqCst),
        0,
        "keeping a projection reads nothing"
    );
    state
        .snapshot()
        .to_canonical_bytes()
        .expect("a heap of projections snapshots")
}

/// No live objects (ADR 0132 §9): a heap that holds projection values
/// serializes and decodes into a fresh state with nothing in memory from the
/// first, and every value reads through the provider of the execution that
/// reads it.
#[tokio::test(flavor = "current_thread")]
async fn a_heap_of_projections_restores_fresh_and_reads_through_its_provider() {
    let bytes = kept_projections_snapshot().await;

    let fresh = Arc::new(Rows::default());
    let mut state =
        State::from_snapshot(Snapshot::from_canonical_bytes(&bytes).expect("snapshot decodes"));
    let value = run(
        read_kept(),
        &mut state,
        ProjectedBindings::new().with_reader(catalog_of(Some(fresh.clone()))),
    )
    .await
    .expect("restored projections read");

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::Number(2.0),
                Value::Number(2.0),
                Value::String("second".into()),
            ]
            .into()
        )
    );
    assert!(
        fresh.reads.load(Ordering::SeqCst) >= 3,
        "every read went to the provider that answers the restored state"
    );
}

/// Refusal (ADR 0132 §9): a projection whose type has no provider is refused
/// with `NoProvider`, never read as a placeholder.
#[tokio::test(flavor = "current_thread")]
async fn a_projection_without_a_provider_is_refused_with_no_provider() {
    let bytes = kept_projections_snapshot().await;
    let mut state =
        State::from_snapshot(Snapshot::from_canonical_bytes(&bytes).expect("snapshot decodes"));

    let error = run(
        read_kept(),
        &mut state,
        ProjectedBindings::new().with_reader(catalog_of(None)),
    )
    .await
    .expect_err("no provider answers `rows`");

    assert_eq!(
        error,
        RuntimeError::ProjectionRefused {
            name: "rows".into(),
            refusal: ProjectionRefusal::NoProvider {
                projection: ProjectionType::new("rows"),
            },
        }
    );
}

/// A catalog holds one provider per type.
#[test]
fn a_catalog_refuses_a_second_provider_of_a_type() {
    let mut catalog = ProjectionCatalog::new();
    catalog
        .register(Arc::new(Rows::default()))
        .expect("the first provider registers");
    assert_eq!(
        catalog.register(Arc::new(Rows::default())).err(),
        Some(ProjectionRefusal::Duplicate {
            projection: ProjectionType::new("rows"),
        })
    );
}

struct LengthAnswer(ProjectedReadResponse);

impl ProjectionReader for LengthAnswer {
    fn read(
        &self,
        _resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, crate::ProjectionReadError> {
        Ok(matches!(request, ProjectedReadRequest::Len).then(|| self.0.clone()))
    }

    fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, crate::ProjectionReadError> {
        requests
            .into_iter()
            .map(|request| self.read(resource, request))
            .collect()
    }
}

/// FIG-5235: every supported length answer means the same to len and rendering.
#[tokio::test(flavor = "current_thread")]
async fn projection_len_and_renderer_agree_on_length_answers() {
    use lash_render::{RenderNode, RenderValue as _};
    for answer in [
        ProjectedReadResponse::Len(2),
        ProjectedReadResponse::Value(Value::Number(2.0)),
        ProjectedReadResponse::Value(Value::List(vec![Value::Null; 2].into())),
        ProjectedReadResponse::Text("éx".into()),
        ProjectedReadResponse::Keys(vec!["a".into(), "b".into()]),
    ] {
        super::super::projection_provider::reading_through(
            Some(Arc::new(LengthAnswer(answer.clone()))), async {
                let projected = rows_value();
                let length = projected.len().expect("supported length answer");
                assert_eq!(length, 2, "{answer:?}");
                assert!(matches!(Value::Projected(projected).node(), RenderNode::Array(n) if n == length), "renderer disagrees for {answer:?}");
            }
        ).await;
    }
}
