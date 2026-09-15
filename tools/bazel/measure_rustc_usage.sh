#!/usr/bin/env bash
# RUSTC_WRAPPER shim that measures one rustc invocation.
#
# Cargo invokes a wrapper as `<wrapper> <rustc> <args...>`. This script passes
# every argument through untouched, exits with rustc's own status, and appends
# one line per invocation to $LASH_RUSTC_USAGE_LOG:
#
#   crate=<--crate-name> kind=<lib|bin|test|build-script> <rss_kb> <user> <sys> <wall>
#
# rss_kb is GNU time's %M (maximum resident set size in KiB). If /usr/bin/time
# is missing or unusable the build still runs; only the measurement is lost.
set -u

rustc_argv=("$@")

log=${LASH_RUSTC_USAGE_LOG:-}
time_bin=/usr/bin/time

if [ -z "$log" ] || [ ! -x "$time_bin" ]; then
  exec "${rustc_argv[@]}"
fi

crate=
crate_types=()
is_test=0
i=1
while [ "$i" -lt "${#rustc_argv[@]}" ]; do
  argument=${rustc_argv[$i]}
  case "$argument" in
    --crate-name)
      i=$((i + 1))
      crate=${rustc_argv[$i]:-}
      ;;
    --crate-name=*)
      crate=${argument#--crate-name=}
      ;;
    --crate-type)
      i=$((i + 1))
      crate_types+=("${rustc_argv[$i]:-}")
      ;;
    --crate-type=*)
      crate_types+=("${argument#--crate-type=}")
      ;;
    --test)
      is_test=1
      ;;
  esac
  i=$((i + 1))
done

# A probe invocation (`rustc -vV`, `--print cfg`) names no crate and compiles
# nothing; it is not an action the pool ever schedules.
if [ -z "$crate" ]; then
  exec "${rustc_argv[@]}"
fi

kind="lib"
if [ "$is_test" -eq 1 ]; then
  kind="test"
elif [ "$crate" = build_script_build ]; then
  kind="build-script"
else
  for crate_type in ${crate_types[@]+"${crate_types[@]}"}; do
    if [ "$crate_type" = bin ]; then
      kind="bin"
    fi
  done
fi

measurement=$(mktemp "${TMPDIR:-/tmp}/measure-rustc.XXXXXXXX" 2>/dev/null) || {
  exec "${rustc_argv[@]}"
}

"$time_bin" -f '%M %U %S %e' -o "$measurement" -- "${rustc_argv[@]}"
status=$?

# On a non-zero exit GNU time prints "Command exited with non-zero status N"
# ahead of the format line, so the measurement is always the last line.
read -r rss_kb user sys wall < <(tail -n 1 "$measurement" 2>/dev/null)
rm -f "$measurement"

# One write, well under PIPE_BUF, so parallel rustc invocations append whole
# lines to the shared log rather than interleaving.
if [[ ${rss_kb:-} =~ ^[0-9]+$ ]]; then
  printf 'crate=%s kind=%s %s %s %s %s\n' \
    "$crate" "$kind" "$rss_kb" "$user" "$sys" "$wall" >>"$log"
fi

exit "$status"
