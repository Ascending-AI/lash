//! Document code and durable native start frames, compiled over the prepared library.
use super::*;

enum UnitSource<'a> {
    Main,
    Function(&'a FunctionId),
    NativeStart(&'a FunctionId),
}

/// Compiles `document`'s own code over `library`, which holds every
/// library body it reaches.
pub(super) fn compile_document(
    document: &Document,
    observed: &Document,
    library: &PreparedLibrary,
    authored: &BTreeSet<FunctionId>,
    imports: &BTreeMap<
        (Unit, lash_vocabulary::ImportSlot),
        (lash_vocabulary::DocumentId, FunctionId),
    >,
) -> Result<DocumentCode, Missing> {
    let prepared = &library.0;
    if !prepared.missing.is_empty() {
        for function in document.functions.keys() {
            if let Some(absent) = prepared.missing.get(function) {
                return Err(Missing(*absent));
            }
        }
    }
    let layout = prepared.layout.shape();
    let mut shuffler = Shuffler(layout.0);
    let mut units: Vec<UnitSource<'_>> = document
        .main
        .as_ref()
        .map(|_| UnitSource::Main)
        .into_iter()
        .collect();
    units.extend(
        document
            .functions
            .iter()
            .filter(|(id, f)| {
                matches!(
                    &f.implementation,
                    doc::Implementation::Guest {
                        metering: doc::Metering::Statements,
                        ..
                    }
                ) && authored.contains(id)
            })
            .map(|(id, _)| UnitSource::Function(id)),
    );
    units.extend(
        document
            .functions
            .iter()
            .filter(|(_, definition)| {
                matches!(
                    definition.implementation,
                    doc::Implementation::Native { .. }
                )
            })
            .map(|(id, _)| UnitSource::NativeStart(id)),
    );
    shuffler.shuffle(layout, &mut units);
    let mut compiler = Compiler::new(
        layout,
        shuffler,
        Resolve {
            library: prepared,
            listed: None,
        },
        &document.private_bindings,
        Base {
            codes: prepared.tables.codes.len() as u32,
            blocks: prepared.tables.blocks.len() as u32,
            stmts: prepared.tables.stmts.len() as u32,
            occurrences: prepared.tables.occurrence_keys.len(),
            sites: prepared.sites,
        },
    );
    compiler.imports = imports
        .iter()
        .filter_map(|(key, (_, function))| {
            let lib = prepared.lib_of(function)?;
            let entry = prepared.lib(lib);
            // Imported natives and uncertified bodies retain ordinary dispatch.
            (!matches!(entry.run, LibRun::Native { .. } | LibRun::Machine(_))
                && matches!(entry.prepared, Some((Entry::Template { .. }, _))))
            .then_some((key.clone(), lib))
        })
        .collect();
    compiler.metered = prepared.charge == crate::Charge::Exact;
    let codes: Vec<CodeId> = units.iter().map(|_| compiler.reserve()).collect();
    let mut main = None;
    let mut native_starts = BTreeMap::new();
    for (unit, code) in units.iter().zip(&codes) {
        match unit {
            UnitSource::Main => main = Some(*code),
            UnitSource::Function(name) => {
                compiler.declared.insert(**name, *code);
            }
            UnitSource::NativeStart(function) => {
                native_starts.insert(**function, *code);
            }
        }
    }
    for (unit, code) in units.iter().zip(codes) {
        match unit {
            UnitSource::Main => compiler.unit_code(
                code,
                Unit::Main,
                &[],
                document.main.as_ref().unwrap_or(&Vec::new()),
                true,
            ),
            UnitSource::NativeStart(function) => {
                let params: Vec<_> = document.functions[*function]
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect();
                // A native's public departure needs a durable frame for its defining settlement hook.
                let block = vec![doc::Stmt::Return {
                    value: doc::Expr::Call {
                        function: **function,
                        args: params.iter().cloned().map(doc::Expr::Variable).collect(),
                    },
                }];
                compiler.unit_code(code, Unit::Library(**function), &params, &block, false);
            }
            UnitSource::Function(name) => {
                if let Some(function) = document.functions.get(*name) {
                    let unit = Unit::Library(**name);
                    let params: Vec<_> = function
                        .signature
                        .params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();
                    if let Some(body) = function.body() {
                        compiler.unit_code(code, unit, &params, &body.block, true);
                    }
                }
            }
        }
    }
    let declared = std::mem::take(&mut compiler.declared);
    let mut tables = compiler.finish();
    let lash_kernel_doc::Ordinals {
        sites: ordinals,
        actions: action_ordinals,
        conditions: condition_sites,
    } = observed.ordinals();
    let block_ordinals = tables
        .blocks
        .iter()
        .map(|block| {
            for (index, stmt) in block.stmts.iter().enumerate() {
                tables.stmts[(stmt.0 as usize) - prepared.tables.stmts.len()].ordinal = ordinals
                    .get(&block.site.child(index as u32))
                    .copied()
                    .and_then(NonZeroU32::new);
            }
            ordinals.get(&block.site).copied()
        })
        .collect();
    let condition_ordinals: BTreeMap<_, _> = condition_sites
        .iter()
        .map(|(site, count)| (ordinals[site], *count))
        .collect();
    let (regions, islands) = if prepared.layout.regions() {
        let base = (
            prepared.tables.codes.len() as u32,
            prepared.tables.blocks.len() as u32,
            prepared.tables.stmts.len() as u32,
        );
        // Islands form first: a loop region runs its guest calls through
        // its callees' islands.
        let islands = islands::form(&mut tables, prepared, base);
        let regions = form_regions(
            &mut tables,
            prepared,
            Costs::of_document(document).loop_test,
            &|code: CodeId| {
                (code.0 as usize)
                    .checked_sub(base.0 as usize)
                    .and_then(|own| islands.by_code.get(own).copied().flatten())
                    .is_some()
            },
        );
        (regions, islands)
    } else {
        Default::default()
    };
    Ok(DocumentCode {
        tables,
        regions,
        islands,
        main,
        declared,
        native_starts,
        block_ordinals,
        site_count: ordinals.len() + action_ordinals.len() + condition_ordinals.len(),
        condition_ordinals,
        action_ordinals,
        reached: std::sync::OnceLock::new(),
    })
}
