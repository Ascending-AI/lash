#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
if [[ "${1:-}" == --help ]]; then
  echo "Run AppendVec's complete test suite under the pinned Miri, with 20 seeds."
  exit 0
fi
if (($#)); then
  echo "usage: scripts/hermetic-build.sh miri [--help]" >&2
  exit 2
fi

mapfile -t pin < <(python3 - <<'PIN'
from pathlib import Path
import tomllib
pin = tomllib.loads(Path("scripts/miri-toolchain.toml").read_text())["toolchain"]
print(pin["channel"])
print(pin["profile"])
print(",".join(pin["components"]))
PIN
)
# Keep the nightly and interpreter sysroot separate from normal build tools,
# including on hosts that do not have Miri installed.
export RUSTUP_HOME="$repo/.tgt/miri/rustup"
export XDG_CACHE_HOME="$repo/.tgt/miri/cache"
rustup toolchain install "${pin[0]}" --profile "${pin[1]}" \
  --component "${pin[2]}" --no-self-update
cargo "+${pin[0]}" miri setup
# The test harness is serial; threads spawned by the tests still interleave.
# Keep Miri's aliasing, race, provenance and leak checks enabled.
export MIRIFLAGS="-Zmiri-many-seeds=0..20"
cargo "+${pin[0]}" miri test --locked -p lash-internal-sansio --lib \
  --target-dir "$repo/.tgt/miri/target" append_vec::tests:: -- --test-threads=1
