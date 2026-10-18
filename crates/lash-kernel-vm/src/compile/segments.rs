//! A generated segment's callback sites as one layout runs them
//! (FIG-6709): each site resolved to where the body's frame stands there,
//! by the coordinate grammar the frame's restore admits (`K-EVAL-008`).
//!
//! A site names the body's own nodes by their paths. Resolution finds the
//! statement the action is nested in, the blocks and loops the frame's
//! control holds to stand there, and the action's resume entry and its
//! roles. A plan one of whose sites or variables the body does not state
//! is refused: the machine runs the body.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::plan::{HelperPlan, PlanLoop};
use lash_kernel_doc::{Site, Unit};
use lash_kernel_exec::ActIx;

use super::{Base, BlockId, CodeId, Form, Local, StmtId, Suffix, Tables};

/// A segment's sites and variables under one layout.
pub(crate) struct Segment {
    /// Each parameter's position in the body's frame.
    pub(crate) params: Vec<u32>,
    pub(crate) sites: Vec<SegmentSite>,
    /// Each site by where the body's frame stands at it: its statement and
    /// the action nested in it, `None` for the statement's own action.
    pub(crate) by_stand: BTreeMap<(StmtId, Option<ActIx>), u32>,
}

/// Where the body's frame stands at one callback site.
pub(crate) struct SegmentSite {
    pub(crate) stmt: StmtId,
    pub(crate) act: Option<ActIx>,
    /// The frame's control there, outermost first.
    pub(crate) control: Vec<SiteControl>,
    /// The action's site as a path under its statement's.
    pub(crate) pending: Suffix,
    /// The operands the statement holds, in the body's evaluation order:
    /// each by its path under the statement's, and the plan register
    /// holding it.
    pub(crate) roles: Vec<(Suffix, u32)>,
    /// The register the action's value goes to.
    pub(crate) dst: u32,
    /// The plan variables in scope there.
    pub(crate) bound: Vec<u32>,
}

/// One entry of a frame's control at a callback site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SiteControl {
    Block {
        block: BlockId,
        next: usize,
    },
    /// A `for`, what it walks in this plan variable.
    For {
        stmt: StmtId,
        walked: u32,
    },
    While {
        stmt: StmtId,
    },
    /// A `try`, standing in its handler or else its body.
    Try {
        stmt: StmtId,
        catch: bool,
    },
}

impl Segment {
    /// Resolves `plan`'s sites against its body `body` in `tables`, the
    /// library's: why it does not resolve, else the segment.
    pub(super) fn resolve(
        plan: &HelperPlan,
        locals: &[Option<Local>],
        body: CodeId,
        tables: &Tables,
        base: &Base,
    ) -> Result<Self, String> {
        let unit = Unit::Library(plan.function);
        let code = tables
            .codes
            .get((body.0 - base.codes) as usize)
            .ok_or("the body has no code")?;
        if let Some((local, _)) = plan
            .slots
            .iter()
            .zip(locals)
            .find(|(local, resolved)| !local.name.is_empty() && resolved.is_none())
        {
            return Err(format!(
                "`{}` at {:?} is no variable",
                local.name, local.path
            ));
        }
        let params = code.params.iter().map(|param| param.at).collect();
        let mut sites = Vec::with_capacity(plan.sites.len());
        let mut by_stand = BTreeMap::new();
        for (index, site) in plan.sites.iter().enumerate() {
            let resolved = resolve_site(tables, base, code.body, &unit, site)
                .map_err(|problem| format!("site {:?}: {problem}", site.path))?;
            if by_stand
                .insert((resolved.stmt, resolved.act), index as u32)
                .is_some()
            {
                return Err(format!("site {:?} is stated twice", site.path));
            }
            sites.push(resolved);
        }
        Ok(Self {
            params,
            sites,
            by_stand,
        })
    }
}

fn resolve_site(
    tables: &Tables,
    base: &Base,
    body: BlockId,
    unit: &Unit,
    site: &lash_kernel_doc::plan::PlanSite,
) -> Result<SegmentSite, String> {
    let path = site.path;
    let block_site = |block: BlockId| &tables.blocks[(block.0 - base.blocks) as usize].site;
    let mut depth = 0;
    let mut block = body;
    let mut control = Vec::new();
    let mut loops = site.loops.iter();
    let (stmt, index) = loop {
        let index = *path.get(depth).ok_or("the path ends at a block")? as usize;
        let stmt = *tables.blocks[(block.0 - base.blocks) as usize]
            .stmts
            .get(index)
            .ok_or("no statement at the path")?;
        control.push(SiteControl::Block {
            block,
            next: index + 1,
        });
        let inner = path
            .get(..depth + 2)
            .map(|inner| Site::new(unit.clone(), inner));
        let at = |candidate: BlockId| inner.as_ref() == Some(block_site(candidate));
        let form = &tables.stmts[(stmt.0 - base.stmts) as usize].form;
        let descended = match form {
            Form::If {
                then_block,
                else_block,
            } => [*then_block, *else_block].into_iter().find(|b| at(*b)),
            Form::For {
                body, else_block, ..
            } => {
                if at(*body) {
                    let Some(PlanLoop::For { walked }) = loops.next() else {
                        return Err("a `for` the site does not state".into());
                    };
                    control.push(SiteControl::For {
                        stmt,
                        walked: *walked,
                    });
                    Some(*body)
                } else {
                    else_block.filter(|b| at(*b))
                }
            }
            Form::While {
                body, else_block, ..
            } => {
                if at(*body) {
                    if loops.next() != Some(&PlanLoop::While) {
                        return Err("a `while` the site does not state".into());
                    }
                    control.push(SiteControl::While { stmt });
                    Some(*body)
                } else {
                    else_block.filter(|b| at(*b))
                }
            }
            Form::Try {
                body,
                catch,
                finally,
                ..
            } => {
                if at(*body) {
                    control.push(SiteControl::Try { stmt, catch: false });
                    Some(*body)
                } else if let Some((_, catch)) = catch.filter(|(_, catch)| at(*catch)) {
                    control.push(SiteControl::Try { stmt, catch: true });
                    Some(catch)
                } else if finally.is_some_and(at) {
                    return Err("a site in a `finally`".into());
                } else {
                    None
                }
            }
            _ => None,
        };
        match descended {
            Some(inner) => {
                block = inner;
                depth += 2;
            }
            None => break (stmt, depth + 1),
        }
    };
    let form = &tables.stmts[(stmt.0 - base.stmts) as usize].form;
    let statement = &path[..index];
    let full = Site::new(unit.clone(), path);
    let (act, pending, roles) = if let Some(act) = tables.stream.nested_at.get(&full) {
        let nested = &tables.stream.nested[act.ix()];
        if nested.depth as usize != index {
            return Err("the action is nested in another statement".into());
        }
        // A `while` waits in its condition: it stands above its loop.
        if matches!(form, Form::While { .. }) {
            if loops.next() != Some(&PlanLoop::While) {
                return Err("a `while` the site does not state".into());
            }
            control.push(SiteControl::While { stmt });
        }
        let mut roles = Vec::with_capacity(nested.roles.len());
        for role in nested.roles.iter() {
            let reg = site
                .roles
                .iter()
                .find(|stated| stated.path == role.site.path.as_slice())
                .ok_or_else(|| format!("the role {:?} is not stated", role.site.path))?
                .reg;
            roles.push((Arc::clone(&role.suffix), reg));
        }
        if roles.len() != site.roles.len() {
            return Err("the site states a role the body does not hold".into());
        }
        (Some(*act), Arc::clone(&nested.suffix), roles)
    } else {
        // The statement's own action: its frame holds no role there.
        let Form::Action(action) = form else {
            return Err("no action at the path".into());
        };
        if action.site != full || path.len() != statement.len() + 1 || !site.roles.is_empty() {
            return Err("the statement's action is elsewhere".into());
        }
        (None, Suffix::from([]), Vec::new())
    };
    if loops.next().is_some() {
        return Err("the site states a loop the body does not hold".into());
    }
    Ok(SegmentSite {
        stmt,
        act,
        control,
        pending,
        roles,
        dst: site.dst,
        bound: site.bound.to_vec(),
    })
}
