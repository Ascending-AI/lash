#!/bin/sh
set -eu

. ./settings.sh

title="${1:-Untitled}"
printf '%s: %s\n' "$PREFIX" "$title"

