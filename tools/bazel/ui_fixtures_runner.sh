#!/usr/bin/env bash
# Direct-rustc runner for the compile-fail UI fixtures (FIG-3364).
#
# Batch mode (the `ui_fixtures_test` executable): reads UI_MANIFEST for the
# toolchain sysroot, the `--extern` set, the `-L` dirs, and the fixture list,
# then compiles every fixture in parallel and diffs normalized stderr against
# `tests/ui/<name>.stderr`. Single mode (`--one <name>`) is invoked per fixture
# by the batch's xargs fan-out; it rebuilds its flags from the environment the
# batch exported.
#
# Fixtures compile from a scratch directory that mirrors the execroot shape
# rustc's remapped diagnostics carry: `crates/`, `external/` and `bazel-out/`
# resolve to the runfiles copies, so dependency sources render as snippets and
# path spellings match what `--remap-path-prefix` baked into the rlibs.
#
# Normalization reproduces trybuild's (src/normalize.rs, preferred variation):
# cargo's `error: aborting`/`--explain`/`could not compile` trailer lines are
# dropped, `crates/lash/` paths become package-relative (`src/...`) for the
# crate under test, other workspace paths become `$WORKSPACE/crates/...`,
# registry sources become `$CARGO/<name>-$VERSION/...`, and files other than
# the fixture itself lose their `:line:col` suffix and snippet line numbers.
set -uo pipefail

manifest="${UI_MANIFEST:?UI_MANIFEST not set}"
package="${UI_PACKAGE:?UI_PACKAGE not set}"
root="${TEST_SRCDIR:?TEST_SRCDIR not set}"

rustc_bin=""
sysroot=""
edition="2024"
lib_dirs=()
rustc_lib_dirs=()
externs=()
fixtures=()

load_manifest() {
    local key value
    while IFS='=' read -r key value; do
        case "$key" in
            rustc) rustc_bin="$root/$value" ;;
            sysroot) sysroot="$root/$value" ;;
            edition) edition="$value" ;;
            lib_dir) lib_dirs+=("$value") ;;
            rustc_lib_dir) rustc_lib_dirs+=("$root/$value") ;;
            extern) externs+=("$value") ;;
            fixture) fixtures+=("$value") ;;
        esac
    done < "$manifest"
}

# rustc resolves librustc_driver relative to its own binary; when the runfiles
# tree materialized the symlinked sysroot bin without its sibling lib/, the
# declared rustc_lib dirs keep the driver reachable.
export_ld_path() {
    local d
    for d in "${rustc_lib_dirs[@]}"; do
        LD_LIBRARY_PATH="${d}${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
    done
    export LD_LIBRARY_PATH
}

rustc_flags() {
    local -n out=$1
    out=(--edition="$edition" --crate-type=bin --cfg trybuild --color=never
         --sysroot="$sysroot" --emit=metadata)
    local d pair name rloc
    for d in "${lib_dirs[@]}"; do
        out+=(-L "dependency=$root/$d")
    done
    for pair in "${externs[@]}"; do
        name="${pair%%=*}"
        rloc="${pair#*=}"
        out+=(--extern "$name=$root/$rloc")
    done
}

# $1 = scratch dir. Links the runfiles tree into the execroot shape diagnostics
# were remapped to: main-repo paths under `crates/`, `bazel-out/` etc., and
# external-repo paths under `external/<canonical repo>/`.
stage_execroot() {
    local stage="$1" entry
    mkdir -p "$stage"
    for entry in "$root/_main"/*; do
        ln -sfn "$entry" "$stage/$(basename "$entry")"
    done
    ln -sfn "$root" "$stage/external"
}

# Stage 1 of normalize(): trybuild's per-line Filter at the preferred
# normalization level.
filter_diag() {
    awk -v input="$1" '
        function hide_leading(line,    n, pad) {
            match(line, /^[ 0-9]+/)
            if (RSTART != 1 || RLENGTH == 0) return line
            pad = sprintf("%*s", RLENGTH, "")
            return pad substr(line, RLENGTH + 1)
        }
        {
            sub(/[ \t]+$/, "")
            if (hide) {
                t = $0; sub(/^[ \t]*/, "", t)
                c = substr(t, 1, 1)
                if (c ~ /[0-9|.]/) {
                    for (i = 0; i < blanks; i++) printf "\n"
                    blanks = 0
                    print hide_leading($0)
                    next
                }
                hide = 0
            }
            if ($0 ~ /^error: aborting due to /) next
            if ($0 ~ /^For more information about (this|an) error/) next
            if ($0 ~ /^Some errors have detailed explanations:/) next
            if ($0 ~ /^error: [Cc]ould not compile `/) next
            if ($0 == "To learn more, run the command again with --verbose.") next
            if ($0 ~ /^= note: this compiler was built on 2/ &&
                $0 ~ /consider upgrading it if it is out of date$/) next
            if ($0 ~ /^= note: the full type name has been written to/) next
            if ($0 ~ /^= note: the full name for the type has been written to/) next
            if ($0 ~ /^[ \t]*and [0-9]+ others$/)
                sub(/[0-9]+ others$/, "$N others")
            if ($0 ~ /^[ \t]*(-->|:::) /) {
                match($0, /^[ \t]*(-->|:::) /)
                pre = substr($0, 1, RLENGTH)
                rest = substr($0, RLENGTH + 1)
                if (sub(/([^ ]*\/)?crates\/lash\//, "", rest)) {
                    if (rest !~ "^" input) {
                        sub(/:[0-9]+:[0-9]+$/, "", rest)
                        hide = 1
                    }
                } else if (sub(/([^ ]*\/)?crates\//, "$WORKSPACE/crates/", rest)) {
                    sub(/:[0-9]+:[0-9]+$/, "", rest)
                    hide = 1
                } else if (sub(/([^ ]*\/)?external\/rules_rs\+\+crate\+crates__/, "$CARGO/", rest)) {
                    sub(/-[0-9][0-9.]*\//, "-$VERSION/", rest)
                    sub(/:[0-9]+:[0-9]+$/, "", rest)
                    hide = 1
                }
                $0 = pre rest
            }
            # Buffer blank lines: they print only before further content, so
            # trailing blanks are trimmed like trybuild trim().
            if ($0 ~ /^[ \t]*$/) { blanks++; next }
            for (i = 0; i < blanks; i++) printf "\n"
            blanks = 0
            print
        }
    '
}

# Stage 2 of normalize(): trybuild's unindent() at the preferred level. After a
# heading line (`error`/`warning`/`note: `), a block whose arrow line is a Code
# line starting with `--> ` has `least_indent` columns cut from every Code and
# Other line in the block (the column cut sits after the first space, so the
# snippet gutter survives). IndentedLineKind:
#   Heading: ^error[:[], ^warning[:[], or `note: ` opening a block
#   Code(n): `... <spaces>` or `<sp><digits><sp>` followed by `|`, `~`/`+`/`-`
#            suggestion, or (no digits) `--> `/`::: `/`= `; n = spaces - 1
#   Note: `note:`, `...`, `help:`, or a >=6-space continuation of a note
#   Other(n): leading spaces (0 if the line carries a line number)
unindent_diag() {
    awk '
        function is_heading(l) {
            return l ~ /^error[:\[]/ || l ~ /^warning[:\[]/ || l ~ /^note: /
        }
        # Returns "H" | "N" | "C" <indent> | "O" <spaces>
        function kind(l, first, prevnote,   spaces, digits, rest, n) {
            if (l ~ /^error[:\[]/ || l ~ /^warning[:\[]/) return "H"
            if (first && l ~ /^note: /) return "H"
            if (l ~ /^note:/ || l == "..." || l ~ /^help:/ ||
                (prevnote && l ~ /^      /)) return "N"
            if (l ~ /^\.\.\. /) {
                rest = substr(l, 5)
                match(rest, /^ +/)
                return "C " RLENGTH
            }
            match(l, /^ */); spaces = RLENGTH
            rest = substr(l, spaces + 1)
            match(rest, /^[0-9]*/); digits = RLENGTH
            rest = substr(rest, digits + 1)
            match(rest, /^ */); spaces += RLENGTH
            rest = substr(rest, RLENGTH + 1)
            if (spaces > 0 && (rest == "|" || rest ~ /^\| / ||
                (digits > 0 && (rest == "~" || rest ~ /^~ / ||
                 rest == "+" || rest ~ /^\+ / ||
                 rest == "-" || rest ~ /^- /)) ||
                (digits == 0 && (rest ~ /^--> / || rest ~ /^::: / ||
                 rest ~ /^= /))))
                return "C " (spaces - 1)
            return "O " (digits == 0 ? spaces : 0)
        }
        { lines[NR] = $0 }
        END {
            i = 1
            while (i <= NR) {
                line = lines[i]
                if (!is_heading(line)) { print line; i++; continue }
                k = kind(lines[i + 1], 0, 0)
                split(k, kv, " ")
                if (kv[1] != "C") { print line; i++; continue }
                indent = kv[2] + 0
                if (substr(lines[i + 1], indent + 2) !~ /^--> /) {
                    print line; i++; continue
                }
                least = indent
                count = 1
                prevnote = 0
                j = i + 2
                while (j <= NR) {
                    k = kind(lines[j], 0, prevnote)
                    split(k, kv, " ")
                    if (kv[1] == "H") break
                    if (kv[1] == "N") { prevnote = 1; count++; j++; continue }
                    prevnote = 0
                    if (kv[1] == "C") {
                        count++
                        if (kv[2] + 0 < least) least = kv[2] + 0
                        j++
                        continue
                    }
                    if (kv[1] == "O" && kv[2] + 0 > 10) { count++; j++; continue }
                    break
                }
                print line
                prevnote = 0
                for (m = i + 1; m <= i + count; m++) {
                    l = lines[m]
                    k = kind(l, 0, prevnote)
                    split(k, kv, " ")
                    if (kv[1] == "N") prevnote = 1; else prevnote = 0
                    if (kv[1] == "C" || kv[1] == "O") {
                        s = index(l, " ")
                        print substr(l, 1, s - 1) substr(l, s + least)
                    } else {
                        print l
                    }
                }
                i += count + 1
            }
        }
    '
}

normalize() {
    filter_diag "$1" | unindent_diag
}

run_one() {
    local name="$1" stage="$2"
    local flags=()
    rustc_flags flags
    local actual status
    actual="$(cd "$stage" && "$rustc_bin" "${flags[@]}" \
        --crate-name="$name" --out-dir "$UI_OUT_DIR/obj" \
        "$package/tests/ui/$name.rs" 2>&1)"
    status=$?
    if [ "$status" -eq 0 ]; then
        echo "FAIL $name: expected a compile failure, but rustc succeeded" \
            > "$UI_OUT_DIR/$name.fail"
        return
    fi
    printf '%s' "$actual" | normalize "tests/ui/$name.rs" \
        > "$UI_OUT_DIR/$name.actual"
    if ! diff -u "$root/_main/$package/tests/ui/$name.stderr" \
            "$UI_OUT_DIR/$name.actual" > "$UI_OUT_DIR/$name.diff"; then
        echo "FAIL $name: diagnostic drift vs tests/ui/$name.stderr" \
            > "$UI_OUT_DIR/$name.fail"
        return
    fi
    echo "PASS $name" > "$UI_OUT_DIR/$name.pass"
}

if [ "${1:-}" = "--one" ]; then
    load_manifest
    export_ld_path
    stage_execroot "${UI_STAGE:?UI_STAGE not set}/$2"
    run_one "$2" "${UI_STAGE}/$2"
    exit 0
fi

load_manifest
export_ld_path
export UI_OUT_DIR="${TEST_TMPDIR:-$(mktemp -d)}/ui-fixtures"
export UI_STAGE="$UI_OUT_DIR/stage"
mkdir -p "$UI_OUT_DIR/obj"

self="$TEST_SRCDIR/_main/tools/bazel/ui_fixtures_runner.sh"
jobs="${UI_JOBS:-8}"
printf '%s\0' "${fixtures[@]}" | xargs -0 -r -P "$jobs" -I {} \
    env UI_MANIFEST="$manifest" UI_PACKAGE="$package" \
        UI_OUT_DIR="$UI_OUT_DIR" UI_STAGE="$UI_STAGE" \
        bash "$self" --one {}

failures=0
for f in "${fixtures[@]}"; do
    if [ -f "$UI_OUT_DIR/$f.pass" ]; then
        echo "PASS $f"
    elif [ -f "$UI_OUT_DIR/$f.fail" ]; then
        failures=$((failures + 1))
        cat "$UI_OUT_DIR/$f.fail"
        cat "$UI_OUT_DIR/$f.diff" 2>/dev/null || true
    else
        failures=$((failures + 1))
        echo "FAIL $f: runner produced no result"
    fi
done

if [ "$failures" -gt 0 ]; then
    echo "$failures of ${#fixtures[@]} UI fixtures drifted"
    exit 1
fi
echo "${#fixtures[@]} UI fixtures match their .stderr pins"
