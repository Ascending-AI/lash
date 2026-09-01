#!/usr/bin/env sh
set -eu

actual="$(sh calc.sh add 2 3)"
if [ "$actual" != "5" ]; then
  echo "expected 5, got $actual" >&2
  exit 1
fi

echo "ok"
