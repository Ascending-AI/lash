#!/bin/sh
set -eu

name="${1:-world}"

. ./greeting.sh
greeting "$name"

