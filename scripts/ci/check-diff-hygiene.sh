#!/usr/bin/env bash
set -euo pipefail

tmp_dir=""
normalize_exit() {
  local status=$?
  trap - EXIT
  if [[ -n "${tmp_dir}" && -d "${tmp_dir}" ]]; then
    rm -f "${tmp_dir}"/*
    rmdir "${tmp_dir}"
  fi
  if ((status == 0)); then
    exit 0
  fi
  exit 1
}
trap normalize_exit EXIT

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"
tmp_dir="$(mktemp -d)"

base_ref="${BASE_REF:-origin/main}"
if [[ -n "${BASE_SHA:-}" ]]; then
  merge_base="${BASE_SHA}"
  base_description="${BASE_SHA}"
else
  if ! merge_base="$(git merge-base "${base_ref}" HEAD)"; then
    echo "Diff hygiene could not resolve merge-base for BASE_REF '${base_ref}'; fetch the base ref or set BASE_REF to a valid ref." >&2
    exit 1
  fi
  base_description="${base_ref}"
fi
range="${merge_base}..HEAD"
diff_range="${merge_base}...HEAD"

if [[ "${DIFF_HYGIENE_BYPASS:-0}" == "1" ]]; then
  echo "Diff hygiene bypassed: DIFF_HYGIENE_BYPASS=1 (the diff-hygiene-override label or an explicit environment override)."
  exit 0
fi

footer_reason=""
declare -a empty_footer_commits=()
if ! git rev-list "${range}" > "${tmp_dir}/commits"; then
  echo "Diff hygiene could not enumerate commits in range '${range}'." >&2
  exit 1
fi
while IFS= read -r commit; do
  if ! git show -s --format=%B "${commit}" | git interpret-trailers --parse > "${tmp_dir}/trailers"; then
    echo "Diff hygiene could not enumerate override footers for commit '${commit}'." >&2
    exit 1
  fi
  while IFS= read -r trailer; do
    [[ "${trailer}" == Bypass-Diff-Hygiene:* ]] || continue
    reason="${trailer#*:}"
    reason="${reason#"${reason%%[![:space:]]*}"}"
    reason="${reason%"${reason##*[![:space:]]}"}"
    if [[ -z "${reason}" ]]; then
      empty_footer_commits+=("$(git rev-parse --short "${commit}")")
    elif [[ -z "${footer_reason}" ]]; then
      footer_reason="${reason}"
    fi
  done < "${tmp_dir}/trailers"
done < "${tmp_dir}/commits"

if ((${#empty_footer_commits[@]})); then
  printf -v empty_commits '%s, ' "${empty_footer_commits[@]}"
  empty_commits="${empty_commits%, }"
  echo "Override-footer validation failed for commit(s) ${empty_commits}: Bypass-Diff-Hygiene has an empty reason; supply a non-empty reason or use DIFF_HYGIENE_BYPASS=1." >&2
  exit 1
fi

if [[ -n "${footer_reason}" ]]; then
  echo "Diff hygiene bypassed by Bypass-Diff-Hygiene footer: ${footer_reason}"
  exit 0
fi

allowlist_file="scripts/ci/diff-hygiene-allowlist.txt"
declare -a allowlist_entries=()
declare -a allowlist_keep=()
keep_next=0
while IFS= read -r line || [[ -n "${line}" ]]; do
  trimmed="${line#"${line%%[![:space:]]*}"}"
  trimmed="${trimmed%"${trimmed##*[![:space:]]}"}"
  if [[ -z "${trimmed}" ]]; then
    keep_next=0
    continue
  fi
  if [[ "${trimmed}" == \#* ]]; then
    if [[ "${trimmed}" == "# keep:"* ]]; then
      keep_next=1
    else
      keep_next=0
    fi
    continue
  fi
  allowlist_entries+=("${trimmed}")
  allowlist_keep+=("${keep_next}")
  keep_next=0
done < "${allowlist_file}"

declare -a failures=()
declare -a allowed_paths=()
for ((i = 0; i < ${#allowlist_entries[@]}; i++)); do
  entry="${allowlist_entries[${i}]}"
  matched=0
  if ! git ls-files -z --cached -- "${entry}" > "${tmp_dir}/tracked-paths"; then
    echo "Diff hygiene could not enumerate tracked paths for allowlist pathspec '${entry}'." >&2
    exit 1
  fi
  while IFS= read -r -d '' tracked_path; do
    matched=1
    allowed_paths+=("${tracked_path}")
  done < "${tmp_dir}/tracked-paths"
  if ((matched == 0 && allowlist_keep[${i}] == 0)); then
    failures+=("Allowlist-rot check failed for pathspec '${entry}': it matches no added-or-tracked path; remove it, precede it with '# keep: reason', or use DIFF_HYGIENE_BYPASS=1.")
  fi
done

is_allowlisted() {
  local path="$1"
  local allowed
  for allowed in ${allowed_paths[@]+"${allowed_paths[@]}"}; do
    if [[ "${allowed}" == "${path}" ]]; then
      return 0
    fi
  done
  return 1
}

declare -a added_paths=()
if ! git diff --diff-filter=A --name-only -z "${diff_range}" > "${tmp_dir}/added-paths"; then
  echo "Diff hygiene could not enumerate added paths in range '${diff_range}'." >&2
  exit 1
fi
while IFS= read -r -d '' path; do
  added_paths+=("${path}")
done < "${tmp_dir}/added-paths"

if ((${#added_paths[@]} > 200)); then
  printf -v added_list "'%s', " "${added_paths[@]}"
  added_list="${added_list%, }"
  failures+=("Check C (added-file count) failed for ${#added_paths[@]} added paths: ${added_list}; reduce the change or use Bypass-Diff-Hygiene: <reason> / DIFF_HYGIENE_BYPASS=1.")
fi

declare -a submodule_paths=()
if ! git diff --diff-filter=A --raw -z "${diff_range}" > "${tmp_dir}/raw-additions"; then
  echo "Diff hygiene could not enumerate added Git entries in range '${diff_range}'." >&2
  exit 1
fi
while IFS= read -r -d '' raw_header; do
  IFS= read -r -d '' path || true
  if [[ "${raw_header}" =~ ^:[0-9]{6}[[:space:]]160000[[:space:]] ]]; then
    submodule_paths+=("${path}")
    failures+=("Check D (new submodule) failed for '${path}'; remove the submodule or use Bypass-Diff-Hygiene: <reason> / DIFF_HYGIENE_BYPASS=1.")
  fi
done < "${tmp_dir}/raw-additions"

is_submodule() {
  local path="$1"
  local submodule
  for submodule in ${submodule_paths[@]+"${submodule_paths[@]}"}; do
    if [[ "${submodule}" == "${path}" ]]; then
      return 0
    fi
  done
  return 1
}

is_junk_path() {
  local path="$1"
  local basename="${path##*/}"
  if [[ "${path}" =~ (^|/)(\.pnpm-store|node_modules|dist|build)(/|$) ]]; then
    return 0
  fi
  if [[ "${path}" =~ (^|/)(\.mypy_cache|\.ruff_cache|\.turbo|target)/ ]]; then
    return 0
  fi
  case "${basename}" in
    .DS_Store|*.log|._*) return 0 ;;
  esac
  return 1
}

byte_limit=$((500 * 1024))
for path in ${added_paths[@]+"${added_paths[@]}"}; do
  if is_allowlisted "${path}"; then
    continue
  fi
  if is_junk_path "${path}"; then
    failures+=("Check A (junk-path ancestry denylist) failed for '${path}'; add a matching pathspec to ${allowlist_file} or use Bypass-Diff-Hygiene: <reason> / DIFF_HYGIENE_BYPASS=1.")
  fi
  if ! is_submodule "${path}"; then
    if ! size="$(git cat-file -s "HEAD:${path}")"; then
      failures+=("Check B (500 KB byte cap) could not read the HEAD blob for '${path}'; fix the Git entry, add a matching pathspec to ${allowlist_file}, or use Bypass-Diff-Hygiene: <reason> / DIFF_HYGIENE_BYPASS=1.")
    elif ((size > byte_limit)); then
      failures+=("Check B (500 KB byte cap) failed for '${path}' at ${size} bytes; add a matching pathspec to ${allowlist_file} or use Bypass-Diff-Hygiene: <reason> / DIFF_HYGIENE_BYPASS=1.")
    fi
  fi
done

declare -a conflict_paths=()
current_path=""
if ! git diff --unified=0 --no-color --no-ext-diff "${diff_range}" -- \
  . ':(exclude,glob)**/*.md' ':(exclude,glob)*.md' > "${tmp_dir}/added-lines"; then
  echo "Diff hygiene could not enumerate added lines in range '${diff_range}'." >&2
  exit 1
fi
while IFS= read -r line || [[ -n "${line}" ]]; do
  if [[ "${line}" == "+++ b/"* ]]; then
    current_path="${line#+++ b/}"
    continue
  fi
  case "${line}" in
    '+<<<<<<< '*|'+>>>>>>> '*|'+======='|'+======= '*)
      already_reported=0
      for path in ${conflict_paths[@]+"${conflict_paths[@]}"}; do
        if [[ "${path}" == "${current_path}" ]]; then
          already_reported=1
          break
        fi
      done
      if ((already_reported == 0)); then
        conflict_paths+=("${current_path}")
        failures+=("Check E (conflict markers in added lines) failed for '${current_path}'; remove the marker or use Bypass-Diff-Hygiene: <reason> / DIFF_HYGIENE_BYPASS=1.")
      fi
      ;;
  esac
done < "${tmp_dir}/added-lines"

if ((${#failures[@]})); then
  printf '%s\n' "${failures[@]}" >&2
  exit 1
fi

echo "Diff hygiene passed: ${#added_paths[@]} added file(s) checked against ${base_description}."
