//! Text projections for review: sites keep their source, and library calls
//! carry both the content id and the display name/label a host joins.
use super::*;

pub(super) fn source_map(
    lowered: &Lowered,
    library: &dyn lash_kernel_dialect::Library,
    source: &str,
    allowed: &[String],
) -> Result<String> {
    let document = &lowered.document;
    let map = lowered
        .source_map
        .as_ref()
        .context("SM-COVER: authored source has a map")?;
    map.validate(document, source)?;
    let mut text = String::new();
    writeln!(
        text,
        "language: {:?}\nexceptions: {}",
        map.language,
        if allowed.is_empty() {
            "none".to_owned()
        } else {
            allowed.join(", ")
        }
    )?;
    let mapped: BTreeSet<_> = map
        .sites
        .iter()
        .filter(|site| !site.ranges.is_empty())
        .map(|site| &site.site)
        .collect();
    for located in &map.sites {
        write!(text, "{}", located.site)?;
        for span in &located.ranges {
            write!(
                text,
                " [{}..{}] {:?}",
                span.start,
                span.end,
                &source[span.start..span.end]
            )?;
        }
        if let Some(offset) = located.implicit {
            write!(text, " implicit={offset}")?;
        }
        text.push('\n');
    }
    for site in code_sites(document, library)
        .into_iter()
        .filter(|site| !mapped.contains(site))
    {
        // Only these structurally recognized, unspelled dialect forms may
        // be excepted. An arbitrary unmapped statement never gets a label.
        let kind = match document.node(&site) {
            Some(Node::Stmt(k::Stmt::Let {
                value: k::Rhs::Expr(Expr::Literal(k::Literal::Absent)),
                ..
            })) if site.unit == Unit::Main && site.path.len() == 1 => {
                "absent-binding initialization"
            }
            _ if task_finalizer(document, library, &site) => "task finalizer",
            _ => anyhow::bail!(
                "SM-COVER: unexplained unmapped site {site}: {:?}",
                document.node(&site)
            ),
        };
        ensure!(
            allowed.iter().any(|allowed| allowed == kind),
            "SM-COVER: undeclared {kind} at {site}"
        );
        writeln!(text, "exception {site}: {kind}")?;
    }
    Ok(text)
}

fn task_finalizer(
    document: &Document,
    library: &dyn lash_kernel_dialect::Library,
    site: &Site,
) -> bool {
    // An entry declares its cleanup in its start convention; only a cell's
    // root wraps it.
    let body = match &site.unit {
        Unit::Main => match document.main.as_ref() {
            Some(body) => body,
            None => return false,
        },
        _ => return false,
    };
    let Some(last) = body
        .len()
        .checked_sub(1)
        .and_then(|index| u32::try_from(index).ok())
    else {
        return false;
    };
    let Some(k::Stmt::Try(k::TryStmt {
        catch: None,
        finally: Some(finally),
        ..
    })) = body.last()
    else {
        return false;
    };
    // Only the dialect's root cancellation is unspelled. An arbitrary
    // finalizer, or cleanup in an ordinary callee, still needs source.
    let [
        k::Stmt::Do {
            value: Expr::Act(action),
        },
    ] = finally.as_slice()
    else {
        return false;
    };
    let is_cleanup = match &action.kind {
        k::ActKind::Call(k::Callee::Local(id)) => document
            .functions
            .get(id)
            .is_some_and(|function| function.name.as_str() == "tasks.cancel_all"),
        k::ActKind::Call(k::Callee::Import(slot)) => {
            document.imports.get(slot).is_some_and(|import| {
                library
                    .imported(&import.name)
                    .is_some_and(|name| name.as_str() == "tasks.cancel_all")
            })
        }
        _ => false,
    };
    action.args.is_empty()
        && is_cleanup
        && (site.path == [last] || site.path.starts_with(&[last, 1]))
}

pub(super) fn view(
    checked: &WorkflowDocument,
    view: &lash_kernel_view::WorkflowView,
) -> Result<String> {
    use lash_kernel_view::CalleeView;
    let mut text = format!("unit {:?}\n", checked.entry_unit());
    for statement in &view.statements {
        writeln!(
            text,
            "statement {} ordinal={} depth={} block={} {:?}",
            statement.site,
            statement.ordinal,
            statement.depth,
            statement.block,
            statement.statement
        )?;
    }
    for scope in &view.scopes {
        writeln!(
            text,
            "scope {} ordinal={} depth={} {:?}",
            scope.block, scope.ordinal, scope.depth, scope.kind
        )?;
    }
    for action in &view.dataflow.actions {
        write!(text, "action {}", action.site)?;
        for callable in &action.applies {
            write!(text, " applies={callable}")?;
        }
        match &action.callee {
            Some(CalleeView::Library {
                id, name, exports, ..
            }) => writeln!(text, " library id={id} name={name} exports={exports:?}")?,
            other => writeln!(text, " {other:?}")?,
        }
    }
    Ok(text)
}

/// Each caption where a canvas draws it: by node, by control arm, by
/// callable, whose entry caption is the workflow's title, and by the entry's
/// parameter and field.
pub(super) fn captions(
    checked: &lash_kernel_check::WorkflowDocument,
    view: &lash_kernel_view::WorkflowView,
    captions: &lash_kernel_doc::Captions,
) -> Result<String> {
    let joined = view.captions(checked, captions)?;
    let mut text = String::new();
    for caption in &joined.callables {
        writeln!(
            text,
            "caption callable {} block={} {}",
            caption.callable,
            caption.block,
            spelled(&caption.caption)
        )?;
    }
    for caption in &joined.nodes {
        writeln!(
            text,
            "caption node {} site={} {}",
            caption.node,
            caption.site,
            spelled(&caption.caption)
        )?;
    }
    for caption in &joined.arms {
        writeln!(
            text,
            "caption arm {}.{} block={} {}",
            caption.node,
            caption.arm,
            caption.block,
            spelled(&caption.caption)
        )?;
    }
    for caption in &joined.params {
        writeln!(
            text,
            "caption param {} {} {}",
            caption.index,
            caption.name,
            spelled(&caption.caption)
        )?;
    }
    for caption in &joined.fields {
        writeln!(
            text,
            "caption field {} {}",
            caption.path,
            spelled(&caption.caption)
        )?;
    }
    Ok(text)
}

fn spelled(caption: &lash_kernel_doc::Caption) -> String {
    match caption.description() {
        Some(description) => format!("{:?} {:?}", caption.title(), description),
        None => format!("{:?}", caption.title()),
    }
}
