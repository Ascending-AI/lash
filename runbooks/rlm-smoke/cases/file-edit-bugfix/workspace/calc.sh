#!/usr/bin/env sh
set -eu

if [ "$#" -ne 3 ] || [ "$1" != "add" ]; then
  echo "usage: sh calc.sh add A B" >&2
  exit 2
fi

a="$2"
b="$3"
echo $((a - b))
