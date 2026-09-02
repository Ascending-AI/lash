#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

declare -a scanned_src_dirs=()
for manifest in crates/*/Cargo.toml; do
  crate_dir="${manifest%/Cargo.toml}"
  if [[ ! -f "$crate_dir/src/lib.rs" ]] && ! grep -Eq '^\[lib\][[:space:]]*$' "$manifest"; then
    continue
  fi

  # Cargo's publish=false and publish=[] both mean that this package is not a
  # published library. All other crates/* libraries are in the boundary scan.
  if grep -Eq '^[[:space:]]*publish[[:space:]]*=[[:space:]]*(false|\[\])[[:space:]]*$' "$manifest"; then
    continue
  fi

  # lash-tools owns cwd-root and shell-runtime access; FIG-1739 notes that
  # crate's boundary ruling is still pending, so exclude its complete source.
  if [[ "$crate_dir" == "crates/lash-tools" ]]; then
    continue
  fi
  scanned_src_dirs+=("$crate_dir/src")
done

if [[ ${#scanned_src_dirs[@]} -eq 0 ]]; then
  echo "core UI boundary check failed: no published library source trees found" >&2
  exit 1
fi

forbidden='(^|[^[:alnum:]_])(PluginSurfaceEvent|PromptRequest|PromptResponse|PromptSelectionMode|PromptPanel|PromptSlot::Cli|CliAutonomous|CliRlm|PanelUpsert|PanelAppend|PanelClear|ModeIndicator|desktop_notification)([^[:alnum:]_]|$)'
ambient_forbidden='env::(current_dir|args|args_os|var|var_os|vars|vars_os)[[:space:]]*\(|(^|[^.[:alnum:]_])current_dir[[:space:]]*\(|(^|[^.[:alnum:]_])stdin[[:space:]]*\(|home_dir|dirs(_next)?::|directories::|atty|IsTerminal'

if command -v rg >/dev/null 2>&1; then
  search=(rg -n --glob '!**/bin/**' --glob '!**/main.rs' "$forbidden" "${scanned_src_dirs[@]}")
else
  # Portable fallback when ripgrep is unavailable: grep -E over the same source
  # trees. -r recurses like rg's directory walk, -n prints line numbers, so the
  # match output and the pass/fail behavior are equivalent.
  search=(grep -rEn --exclude-dir=bin --exclude=main.rs "$forbidden" "${scanned_src_dirs[@]}")
fi

if "${search[@]}"; then
  echo "core UI boundary check failed: UI-only vocabulary found in published library source" >&2
  exit 1
else
  search_status=$?
  if [[ $search_status -ne 1 ]]; then
    echo "core UI boundary check failed: source search exited $search_status" >&2
    exit "$search_status"
  fi
fi

if command -v rg >/dev/null 2>&1; then
  ambient_search=(rg -n --glob '!**/bin/**' --glob '!**/main.rs' "$ambient_forbidden" "${scanned_src_dirs[@]}")
else
  ambient_search=(grep -rEn --exclude-dir=bin --exclude=main.rs "$ambient_forbidden" "${scanned_src_dirs[@]}")
fi

ambient_hits="$(mktemp)"
ambient_filtered_hits="$(mktemp)"
trap 'rm -f "$ambient_hits" "$ambient_filtered_hits"' EXIT

if "${ambient_search[@]}" >"$ambient_hits"; then
  :
else
  search_status=$?
  if [[ $search_status -ne 1 ]]; then
    echo "core UI boundary check failed: source search exited $search_status" >&2
    exit "$search_status"
  fi
fi

# Explicit ambient carve-outs:
# - lash-rlm-types/src/lib.rs:440 reads the runbook dialect required by ADR 0066.
# - lash-s3-store/src/lib.rs:637,641,649,657,658,665,668,671 are temporary
#   FIG-1739 exclusions for the existing MinIO configuration reads. FIG-2420
#   removes those reads and this complete exclusion.
while IFS= read -r hit; do
  [[ -z "$hit" ]] && continue
  hit_path="${hit%%:*}"
  hit_rest="${hit#*:}"
  hit_line="${hit_rest%%:*}"
  case "$hit_path:$hit_line" in
    crates/lash-rlm-types/src/lib.rs:440|\
    crates/lash-s3-store/src/lib.rs:637|\
    crates/lash-s3-store/src/lib.rs:641|\
    crates/lash-s3-store/src/lib.rs:649|\
    crates/lash-s3-store/src/lib.rs:657|\
    crates/lash-s3-store/src/lib.rs:658|\
    crates/lash-s3-store/src/lib.rs:665|\
    crates/lash-s3-store/src/lib.rs:668|\
    crates/lash-s3-store/src/lib.rs:671)
      continue
      ;;
  esac
  printf '%s\n' "$hit" >>"$ambient_filtered_hits"
done <"$ambient_hits"

# Test-only reads are mechanically excluded: tests.rs and *_tests.rs, paths
# marked tests or testing by the crate layout, and lexically detected
# #[cfg(test)] mod blocks.
# The detector strips Rust comments and literals before matching braces, so a
# deliberate production env::var has no test-module span and remains visible.
test_violations="$(python3 - "$ambient_filtered_hits" <<'PY'
import pathlib
import re
import sys


def code_without_comments_or_literals(source: str) -> str:
    output = []
    index = 0
    state = "code"
    raw_hashes = 0

    while index < len(source):
        char = source[index]
        next_char = source[index + 1] if index + 1 < len(source) else ""
        if state == "line_comment":
            if char == "\n":
                state = "code"
                output.append(char)
            else:
                output.append(" ")
            index += 1
            continue
        if state == "block_comment":
            if char == "*" and next_char == "/":
                output.extend("  ")
                index += 2
                state = "code"
            else:
                output.append("\n" if char == "\n" else " ")
                index += 1
            continue
        if state == "string":
            if char == "\\":
                output.extend("  ")
                index += 2
            elif char == '"':
                output.append(" ")
                index += 1
                state = "code"
            else:
                output.append("\n" if char == "\n" else " ")
                index += 1
            continue
        if state == "char":
            if char == "\\":
                output.extend("  ")
                index += 2
            elif char == "'":
                output.append(" ")
                index += 1
                state = "code"
            else:
                output.append("\n" if char == "\n" else " ")
                index += 1
            continue
        if state == "raw_string":
            terminator = '"' + ("#" * raw_hashes)
            if source.startswith(terminator, index):
                output.extend(" " * len(terminator))
                index += len(terminator)
                state = "code"
            else:
                output.append("\n" if char == "\n" else " ")
                index += 1
            continue

        if char == "/" and next_char == "/":
            output.extend("  ")
            index += 2
            state = "line_comment"
        elif char == "/" and next_char == "*":
            output.extend("  ")
            index += 2
            state = "block_comment"
        elif char == '"':
            output.append(" ")
            index += 1
            state = "string"
        elif char == "'" and not (index + 1 < len(source) and source[index + 1].isalnum()):
            output.append(" ")
            index += 1
            state = "char"
        elif char == "r":
            match = re.match(r'r(#+)?"', source[index:])
            if match:
                raw_hashes = len(match.group(1) or "")
                output.extend(" " * len(match.group(0)))
                index += len(match.group(0))
                state = "raw_string"
            else:
                output.append(char)
                index += 1
        else:
            output.append(char)
            index += 1

    return "".join(output)


def test_module_ranges(path: pathlib.Path):
    source = path.read_text()
    code = code_without_comments_or_literals(source)
    ranges = []
    for cfg in re.finditer(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]", code):
        module = re.search(r"\bmod\s+[A-Za-z_][A-Za-z0-9_]*\b[^;{]*\{", code[cfg.end():])
        if not module:
            continue
        opening = cfg.end() + module.end() - 1
        depth = 0
        closing = None
        for index in range(opening, len(code)):
            if code[index] == "{":
                depth += 1
            elif code[index] == "}":
                depth -= 1
                if depth == 0:
                    closing = index
                    break
        if closing is not None:
            ranges.append((opening, closing))
    return source, ranges


def is_test_only(path_text: str, line_number: int) -> bool:
    path = pathlib.Path(path_text)
    normalized = path.as_posix()
    if path.name in {"tests.rs"} or path.name.endswith("_tests.rs") or "/tests/" in normalized or "/testing/" in normalized:
        return True
    source, ranges = test_module_ranges(path)
    line_start = source.splitlines(keepends=True)
    offset = sum(len(line) for line in line_start[: line_number - 1])
    return any(start <= offset <= end for start, end in ranges)


for raw_hit in pathlib.Path(sys.argv[1]).read_text().splitlines():
    path_text, line_text, _ = raw_hit.split(":", 2)
    if not is_test_only(path_text, int(line_text)):
        print(raw_hit)
PY
)"

if [[ -n "$test_violations" ]]; then
  printf '%s\n' "$test_violations" >&2
  echo "core UI boundary check failed: ambient-world capture found in published library source" >&2
  exit 1
fi
