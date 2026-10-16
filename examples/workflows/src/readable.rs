//! Text projections for review: sites keep their source, and library calls
//! carry both the content id and the display name/label a host joins.
use super::*;

pub(super) fn source_map(lowered: &Lowered, source: &str, allowed: &[String]) -> Result<String> {
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
    for site in code_sites(document)
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
            _ if task_finalizer(document, &site) => "task finalizer",
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

fn task_finalizer(document: &Document, site: &Site) -> bool {
    let Some(last) = document
        .main
        .len()
        .checked_sub(1)
        .and_then(|index| u32::try_from(index).ok())
    else {
        return false;
    };
    matches!(
        document.main.last(),
        Some(k::Stmt::Try(k::TryStmt {
            catch: None,
            finally: Some(_),
            ..
        }))
    ) && site.unit == Unit::Main
        && (site.path == [last] || site.path.starts_with(&[last, 1]))
}

pub(super) fn view(
    checked: &WorkflowDocument,
    view: &lash_kernel_view::WorkflowView,
) -> Result<String> {
    use lash_kernel_view::CalleeView;
    let labels = lash_helper_labels::labels()?;
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
        match &action.callee {
            Some(CalleeView::Library { id, name }) => writeln!(
                text,
                " library id={id} name={name} label={:?}",
                lash_helper_labels::title(&labels, name)
            )?,
            other => writeln!(text, " {other:?}")?,
        }
    }
    Ok(text)
}
