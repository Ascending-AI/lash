//! Content-id presence in the one index (cutover contract §6 index rule): a
//! tuple's references carry content ids and every other index's carry
//! none, at the owner's embedded top and at every page below it alike. A
//! reference forged the other way, or with a digest count its arity does
//! not declare, is refused wherever it is read.
use super::super::run::chunked::{ChunkRef, Chunked};
use super::super::run::index::{Index, Part, Seek, Shape};
use super::super::run::objects::ObjectKind;
use super::super::tree::Access;
use super::pages::fresh;
use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_value::tokens::{TokenCodec, TokenReader, TokenWriter, decode_body, encode_body};

type Key = <ChunkRef as super::super::run::index::Leaf>::Key;

/// More leaves than one node of nodes holds: the owner's top is at height
/// two, so a read passes an index page of parts, then one of leaves.
const LEAVES: u64 = 96 * 96 + 84;

fn leaves(tuple: bool) -> Vec<ChunkRef> {
    (0..LEAVES)
        .map(|at| {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&at.to_be_bytes());
            ChunkRef {
                first: None,
                len: 64,
                page: PageRef {
                    id: PageId(id),
                    grant: PageGrant::NONE,
                },
                leaf: tuple.then_some(id),
            }
        })
        .collect()
}

/// `item`'s encoding with one digest more than its arity declares
/// written after it: the same fields, a field count one higher.
fn overlong<T: TokenCodec>(item: &T, out: &mut TokenWriter) {
    let tokens = encode_body(item);
    let mut reader = TokenReader::new(&tokens);
    let fields = reader.seq().unwrap();
    let rest = &tokens[reader.position()..];
    out.seq(fields + 1);
    out.raw(rest);
    out.bytes(&[0xee; 32]);
}

/// The forgeries of one reference: its content id taken off or put on,
/// whichever the shape forbids, and an arity one digest longer.
#[derive(Clone, Copy, Debug)]
enum Forge {
    Flip,
    Overlong,
}

fn flip_part(part: &mut Part<Key>) {
    part.id = match part.id {
        Some(_) => None,
        None => Some([0xdd; 32]),
    };
}

fn flip_leaf(leaf: &mut ChunkRef) {
    leaf.leaf = match leaf.leaf {
        Some(_) => None,
        None => Some([0xdd; 32]),
    };
}

/// A list of `items` whose first is forged.
fn forged_list<T: TokenCodec + Clone>(items: &[T], forge: Forge, flip: impl Fn(&mut T)) -> Vec<u8> {
    let mut out = TokenWriter::new();
    out.seq(items.len() as u64);
    for (at, item) in items.iter().enumerate() {
        match (at, forge) {
            (0, Forge::Flip) => {
                let mut item = item.clone();
                flip(&mut item);
                item.encode(&mut out);
            }
            (0, Forge::Overlong) => overlong(item, &mut out),
            _ => item.encode(&mut out),
        }
    }
    out.into_bytes()
}

/// Reseals the page `part` names with `tokens` and the children it held,
/// one of them swapped for `swap` if given, and names the new page.
fn reseal(
    access: &Access<'_>,
    shape: &Shape,
    part: &mut Part<Key>,
    tokens: Vec<u8>,
    swap: Option<(PageRef, PageRef)>,
) {
    let answer = access.read(part.page).unwrap();
    let children = answer
        .children
        .iter()
        .map(|child| match swap {
            Some((old, new)) if child.id == old.id => new,
            _ => *child,
        })
        .collect();
    part.page = access.seal(shape.kind, tokens, children).unwrap();
}

fn refused(access: &Access<'_>, shape: &Shape, index: &Index<ChunkRef>) -> bool {
    index.check(shape).is_err() || index.get(access, shape, Seek::At(0)).is_err()
}

/// Every forgery at every level, for a tuple's index and a list's.
#[test]
fn content_ids_forged_at_the_owner_or_a_page_are_refused() {
    let mut missed = Vec::new();
    for kind in [ObjectKind::List, ObjectKind::Tuple] {
        let (access, _) = fresh();
        let shape = Chunked::shape_of(kind, LEAVES * 64);
        let index = Index::build(&access, &shape, leaves(kind == ObjectKind::Tuple)).unwrap();
        let Index::Nodes { height: 2, refs } = &index else {
            panic!("the fixture's top is at height two");
        };
        assert!(index.check(&shape).is_ok());
        assert!(index.get(&access, &shape, Seek::At(0)).unwrap().is_some());
        for forge in [Forge::Flip, Forge::Overlong] {
            // The owner's top.
            let owner = match forge {
                Forge::Flip => {
                    let mut refs = refs.clone();
                    flip_part(&mut refs[0]);
                    Some(Index::Nodes { height: 2, refs })
                }
                Forge::Overlong => {
                    let mut out = TokenWriter::new();
                    out.sum(1, 2);
                    out.unsigned(2);
                    out.raw(&forged_list(refs, forge, flip_part));
                    decode_body::<Index<ChunkRef>>(&out.into_bytes()).ok()
                }
            };
            if owner.is_some_and(|owner| !refused(&access, &shape, &owner)) {
                missed.push(format!("{kind:?} owner {forge:?}"));
            }
            // The page of parts the owner's first reference names.
            let parts: Vec<Part<Key>> =
                decode_body(&access.read(refs[0].page).unwrap().tokens).unwrap();
            let mut top = refs.clone();
            reseal(
                &access,
                &shape,
                &mut top[0],
                forged_list(&parts, forge, flip_part),
                None,
            );
            if !refused(
                &access,
                &shape,
                &Index::Nodes {
                    height: 2,
                    refs: top,
                },
            ) {
                missed.push(format!("{kind:?} node of parts {forge:?}"));
            }
            // The page of leaves under it.
            let leaves: Vec<ChunkRef> =
                decode_body(&access.read(parts[0].page).unwrap().tokens).unwrap();
            let mut middle = parts.clone();
            let old = middle[0].page;
            reseal(
                &access,
                &shape,
                &mut middle[0],
                forged_list(&leaves, forge, flip_leaf),
                None,
            );
            let mut top = refs.clone();
            reseal(
                &access,
                &shape,
                &mut top[0],
                encode_body(&middle),
                Some((old, middle[0].page)),
            );
            if !refused(
                &access,
                &shape,
                &Index::Nodes {
                    height: 2,
                    refs: top,
                },
            ) {
                missed.push(format!("{kind:?} node of leaves {forge:?}"));
            }
        }
    }
    assert!(
        missed.is_empty(),
        "forged content ids read back: {missed:?}"
    );
}
