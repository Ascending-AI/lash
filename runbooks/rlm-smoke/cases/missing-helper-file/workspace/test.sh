#!/bin/sh
set -eu

actual="$(sh main.sh Ada)"
if [ "$actual" != "Hello, Ada!" ]; then
  echo "expected Hello, Ada!, got $actual" >&2
  exit 1
fi

echo ok

