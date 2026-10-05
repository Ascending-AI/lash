#!/usr/bin/env bash
# Moves the one full Confidence build from its producer job to every stage job.
#
#   confidence-shared-build.sh pack <tools-dir> <build-dir>      producer workspace
#   confidence-shared-build.sh restore <tools-dir> [<build-dir>] consumer workspace
#
# <tools-dir> carries the bootstrapped cargo-mutants and cargo-llvm-cov, which
# every stage needs. <build-dir> carries `target` as zstd tar chunks, which only
# the stages that run the prebuilt tests need: coverage and mutation stages
# build in their own trees and restore the tools alone.
#
# Neither side ever holds the build twice. Pack deletes each file from `target`
# once it is in the archive, and restore deletes each chunk once it is
# extracted, so a runner's peak is the tree plus one chunk. Run 37181695061
# filled the 86 GB its runner had free while still building, and runs
# 36294879308 and 37258510534 filled it writing the archive beside the tree.
set -euo pipefail

usage() {
  echo "usage: $0 pack <tools-dir> <build-dir> | restore <tools-dir> [<build-dir>]" >&2
  exit 2
}

[ "$#" -ge 2 ] || usage
action="$1"
tools_dir="$2"
build_dir="${3:-}"
tools=(cargo-mutants cargo-llvm-cov)
cargo_bin="${CARGO_HOME:-${HOME}/.cargo}/bin"
chunk="build.tar.zst."

if ! command -v zstd >/dev/null 2>&1; then
  echo "zstd is required to ${action} the shared Confidence build" >&2
  exit 127
fi

report_disk() {
  echo "$1:"
  shift
  df -h . "$@" | sed 's/^/  /'
}

case "$action" in
  pack)
    [ "$#" -eq 3 ] || usage
    mkdir -p "$tools_dir" "$build_dir"
    for tool in "${tools[@]}"; do
      cp "${cargo_bin}/${tool}" "$tools_dir/"
    done
    report_disk "disk before packing target ($(du -sh target | cut -f1))" "$build_dir"
    du -h --max-depth=2 target | sort -h | tail -n 12 | sed 's/^/  /'
    tar -I 'zstd -T0 -3' --remove-files -cf - target \
      | split -b 2G -d -a 3 - "${build_dir}/${chunk}"
    report_disk "disk after packing $(du -sh "$build_dir" | cut -f1) into ${build_dir}" "$build_dir"
    ;;
  restore)
    [ "$#" -le 3 ] || usage
    mkdir -p "$cargo_bin"
    for tool in "${tools[@]}"; do
      install -m 0755 "${tools_dir}/${tool}" "${cargo_bin}/${tool}"
    done
    [ -n "$build_dir" ] || exit 0
    report_disk "disk before restoring $(du -sh "$build_dir" | cut -f1) from ${build_dir}"
    parts=("${build_dir}/${chunk}"*)
    [ -e "${parts[0]}" ] || { echo "no ${chunk}* chunks in ${build_dir}" >&2; exit 1; }
    for part in "${parts[@]}"; do
      cat "$part"
      rm -f "$part"
    done | tar -I zstd -xmf -
    report_disk "disk after restoring target ($(du -sh target | cut -f1))"
    ;;
  *) usage ;;
esac
