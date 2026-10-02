#!/usr/bin/env bash
# Moves the one full Confidence build from its producer job to every stage job.
#
#   confidence-shared-build.sh pack <archive>     in the producer's workspace
#   confidence-shared-build.sh restore <archive>  in a consumer's workspace
#
# The archive is compressed as it is written and removed once restored, so no
# runner holds the build twice. Scheduled run 36294879308 wrote an uncompressed
# tar of `target` (29.7 GB a week earlier) beside the tree and ran out of disk.
set -euo pipefail

usage() {
  echo "usage: $0 pack|restore <archive>" >&2
  exit 2
}

[ "$#" -eq 2 ] || usage
action="$1"
archive="$2"
tools=(cargo-mutants cargo-llvm-cov)
cargo_bin="${CARGO_HOME:-${HOME}/.cargo}/bin"

if ! command -v zstd >/dev/null 2>&1; then
  echo "zstd is required to ${action} the shared Confidence build" >&2
  exit 127
fi

report_disk() {
  echo "$1:"
  df -h . "$(dirname "$archive")" | sed 's/^/  /'
}

case "$action" in
  pack)
    mkdir -p target/confidence-tools "$(dirname "$archive")"
    for tool in "${tools[@]}"; do
      cp "${cargo_bin}/${tool}" target/confidence-tools/
    done
    report_disk "disk before packing target ($(du -sh target | cut -f1))"
    tar -I 'zstd -T0 -3' -cf "$archive" target
    report_disk "disk after packing $(du -h "$archive" | cut -f1) into ${archive}"
    ;;
  restore)
    report_disk "disk before restoring ${archive}"
    tar -I zstd -xmf "$archive"
    rm -f "$archive"
    mkdir -p "$cargo_bin"
    for tool in "${tools[@]}"; do
      cp "target/confidence-tools/${tool}" "${cargo_bin}/"
    done
    report_disk "disk after restoring target ($(du -sh target | cut -f1))"
    ;;
  *) usage ;;
esac
