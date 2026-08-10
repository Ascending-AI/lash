#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

forbidden='PluginSurfaceEvent|PromptRequest|PromptResponse|PromptSelectionMode|PromptPanel|PromptSlot::Cli|CliAutonomous|CliRlm|PanelUpsert|PanelAppend|PanelClear|ModeIndicator|desktop_notification'
ambient_forbidden='env::(current_dir|args|args_os)[[:space:]]*\(|(^|[^.[:alnum:]_])current_dir[[:space:]]*\(|(^|[^.[:alnum:]_])stdin[[:space:]]*\(|home_dir|dirs(_next)?::|directories::|atty|IsTerminal'

if command -v rg >/dev/null 2>&1; then
  search=(rg -n "$forbidden" crates/lash-sansio/src crates/lash-core/src crates/lash/src)
else
  # Portable fallback when ripgrep is unavailable: grep -E over the same source
  # trees. -r recurses like rg's directory walk, -n prints line numbers, so the
  # match output and the pass/fail behavior are identical.
  search=(grep -rEn "$forbidden" crates/lash-sansio/src crates/lash-core/src crates/lash/src)
fi

if "${search[@]}"; then
  echo "core UI boundary check failed: UI-only vocabulary found in sansio/core/facade source" >&2
  exit 1
else
  search_status=$?
  if [[ $search_status -ne 1 ]]; then
    echo "core UI boundary check failed: source search exited $search_status" >&2
    exit "$search_status"
  fi
fi

if command -v rg >/dev/null 2>&1; then
  ambient_search=(rg -n "$ambient_forbidden" crates/lash-sansio/src crates/lash-core/src crates/lash/src)
else
  ambient_search=(grep -rEn "$ambient_forbidden" crates/lash-sansio/src crates/lash-core/src crates/lash/src)
fi

if "${ambient_search[@]}"; then
  echo "core UI boundary check failed: ambient-world capture found in sansio/core/facade source" >&2
  exit 1
else
  search_status=$?
  if [[ $search_status -ne 1 ]]; then
    echo "core UI boundary check failed: ambient-world search exited $search_status" >&2
    exit "$search_status"
  fi
fi
