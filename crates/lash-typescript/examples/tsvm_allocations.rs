//! Allocation observations are separate from the uninstrumented timings.
#![allow(clippy::disallowed_methods, clippy::expect_used)]

#[path = "tsvm_costs/allocation.rs"]
mod allocation;
#[path = "monty_comparison.rs"]
#[allow(dead_code)]
mod baseline;
#[path = "tsvm_costs/corpus.rs"]
mod corpus;

use lashlang::{Snapshot, VmContinuation};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;

#[global_allocator]
static ALLOCATOR: allocation::CountingAllocator = allocation::CountingAllocator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    let option = |name: &str, default: &str| {
        arguments
            .windows(2)
            .find(|pair| pair[0] == name)
            .map_or_else(|| default.to_string(), |pair| pair[1].clone())
    };
    let root = PathBuf::from(option("--repo", "."));
    let output = PathBuf::from(option("--output", "tsvm-f0-data"));
    let count: usize = option("--samples", "10000").parse()?;
    assert!(count >= 10000);
    std::fs::create_dir_all(&output)?;
    let mut file = std::fs::File::create(output.join("allocations.csv"))?;
    writeln!(file, "metric,sample,value,unit")?;
    // Also checks that allocator instrumentation actually sees an allocation.
    let before = allocation::total();
    black_box(vec![0_u8; black_box(8192)]);
    assert!(allocation::total() - before >= 8192);
    for case in corpus::load(&root) {
        black_box((&case.source, &case.program));
        let continuation = serde_json::to_vec(&case.continuation)?;
        let snapshot = case.snapshot.to_canonical_bytes()?;
        for index in 0..count {
            let before = allocation::total();
            black_box(serde_json::from_slice::<VmContinuation>(&continuation)?);
            let bytes = allocation::total() - before;
            writeln!(
                file,
                "continuation-decode-allocated-{},{},{},bytes",
                case.id, index, bytes
            )?;
            let before = allocation::total();
            black_box(Snapshot::from_canonical_bytes(&snapshot)?);
            let bytes = allocation::total() - before;
            writeln!(
                file,
                "snapshot-decode-allocated-{},{},{},bytes",
                case.id, index, bytes
            )?;
        }
        println!("{}: {count} allocation samples for each codec", case.id);
    }
    Ok(())
}
