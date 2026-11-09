//! The bytecode core's durable heap format (cutover contract §6): object
//! ids, object records with their exact edge sets, the copy-on-write
//! directory and the run root.
//!
//! An object is written when it is first paged out or sealed, under an
//! [`ObjectId`](ids::ObjectId) from the run root's high-water mark. Its
//! directory row holds its record and the exact set of ids the record
//! references, so reachability over the directory reads rows only, never
//! a large object's member pages.

pub mod chunked;
pub mod continuation;
pub mod directory;
pub mod ids;
pub mod objects;
pub mod root;
