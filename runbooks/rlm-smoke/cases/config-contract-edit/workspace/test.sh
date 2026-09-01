#!/bin/sh
set -eu

actual="$(sh render.sh build-42)"
if [ "$actual" != "release: build-42" ]; then
  echo "expected release: build-42, got $actual" >&2
  exit 1
fi

echo ok

