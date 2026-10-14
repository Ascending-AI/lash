//! Host-owned print, compile and complete-byte comparison through the facade.
//! Also emits repeatable cost measurements; see docs/design/generated-source-cost.md.
use std::error::Error;
use std::time::{Duration, Instant};

use lash::dialect::{PrintOptions, PrintedSource, SourceFormat};
use lash::workflow::document::{self as k, Document, Expr, Literal, Node, Rhs, Stmt};
use lash::workflow::{WorkflowDocument, WorkflowDocumentEntry};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[expect(
    clippy::disallowed_methods,
    reason = "the executable host reads argv in main"
)]
fn main() -> Result<()> {
    // Enter the credential-free worker before creating any host runtime.
    if lash::vm::worker_entry(&lash::vm::built_worker_dialects)? {
        return Ok(());
    }
    let iterations = std::env::args()
        .nth(1)
        .map_or(Ok(20), |arg| arg.parse::<u32>())?;
    if iterations == 0 {
        return Err("iterations must be positive".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure(iterations))
}

fn fixture(width: usize, depth: usize) -> Result<Document> {
    let mut document = k::parse_document("kernel 1\nnumbers float\nmain { finish null }")?;
    let mut body = Vec::new();
    for index in 0..width {
        body.push(Stmt::Let {
            name: k::Name::new(format!("item_{index}")),
            value: Rhs::Expr(Expr::List(vec![
                Expr::Literal(Literal::Int(k::Integer::from(i64::try_from(index)?))),
                Expr::Literal(Literal::Text("readable λ".into())),
            ])),
        });
    }
    for _ in 0..depth {
        body = vec![Stmt::If {
            condition: Expr::Literal(Literal::Bool(true)),
            then_block: body,
            else_block: Vec::new(),
        }];
    }
    body.extend(document.main);
    document.main = body;
    Ok(document)
}

fn count(node: Node<'_>) -> usize {
    1 + node.children().into_iter().map(count).sum::<usize>()
}

fn print_local(
    view: &WorkflowDocument,
    format: SourceFormat,
    options: PrintOptions,
) -> Result<PrintedSource> {
    Ok(match format {
        SourceFormat::TypeScriptV1 => view.typescript(options)?,
        SourceFormat::PythonV1 => view.python(options)?,
    })
}

fn read_local(source: &str, base: &Document, format: SourceFormat) -> Result<Document> {
    Ok(match format {
        SourceFormat::TypeScriptV1 => lash::typescript::read(source, base, Default::default())?,
        SourceFormat::PythonV1 => lash::python::read(source, base, Default::default())?,
    })
}

fn compare(original: &Document, decoded: &Document) -> Result<()> {
    // Compare the complete canonical bytes, including manifest and unused bodies.
    if original.canonical_bytes()? != decoded.canonical_bytes()? {
        return Err("exported source changed the document".into());
    }
    Ok(())
}

async fn measure(iterations: u32) -> Result<()> {
    let service = lash::vm::WorkerService::new(lash::vm::WorkerPoolConfig::codemode(
        lash::vm::WorkerEntry::reexec()?,
    ));
    service.pool()?; // Bootstrap is outside the measured source operations.
    println!(
        "case,path,dialect,maps,nodes,document_bytes,source_bytes,map_bytes,print_us,print_compile_compare_us,iterations"
    );
    for (name, width, depth) in [
        ("small", 1, 0),
        ("typical", 40, 2),
        ("wide", 500, 0),
        ("deep", 1, 20),
    ] {
        let document = fixture(width, depth)?;
        let nodes = count(Node::Block(&document.main));
        let document_bytes = document.canonical_bytes()?.len();
        let view = WorkflowDocument::derive(
            document.clone(),
            WorkflowDocumentEntry::Main,
            &k::FunctionRegistry::default(),
        )?;
        for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
            for maps in [false, true] {
                let options = PrintOptions {
                    source_map: maps,
                    ..Default::default()
                };
                for worker in [false, true] {
                    let mut print_time = Duration::ZERO;
                    let mut composed_time = Duration::ZERO;
                    let mut sizes = (0, 0);
                    for _ in 0..iterations {
                        let start = Instant::now();
                        let printed = if worker {
                            service.print(&document, format, options).await?
                        } else {
                            print_local(&view, format, options)?
                        };
                        print_time += start.elapsed();
                        // Verify the print-only sample outside its timed region.
                        let decoded = if worker {
                            service
                                .read_exported(&printed.source, &document, format, options.limits)
                                .await?
                        } else {
                            read_local(&printed.source, &document, format)?
                        };
                        compare(&document, &decoded)?;
                        sizes = (
                            printed.source.len(),
                            printed
                                .source_map
                                .as_ref()
                                .map(serde_json::to_vec)
                                .transpose()?
                                .map_or(0, |bytes| bytes.len()),
                        );

                        let start = Instant::now();
                        let printed = if worker {
                            service.print(&document, format, options).await?
                        } else {
                            print_local(&view, format, options)?
                        };
                        let decoded = if worker {
                            service
                                .read_exported(&printed.source, &document, format, options.limits)
                                .await?
                        } else {
                            read_local(&printed.source, &document, format)?
                        };
                        compare(&document, &decoded)?;
                        composed_time += start.elapsed();
                    }
                    println!(
                        "{name},{},{},{maps},{nodes},{document_bytes},{},{},{:.3},{:.3},{iterations}",
                        if worker { "worker" } else { "local" },
                        format.dialect(),
                        sizes.0,
                        sizes.1,
                        print_time.as_secs_f64() * 1e6 / f64::from(iterations),
                        composed_time.as_secs_f64() * 1e6 / f64::from(iterations)
                    );
                }
            }
        }
    }
    Ok(())
}
