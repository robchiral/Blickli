#!/bin/sh
# Print the app version, read from blickli/Cargo.toml.
set -eu

cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' blickli/Cargo.toml | head -1)
if [ -z "$VERSION" ]; then
  echo "no version in blickli/Cargo.toml" >&2
  exit 1
fi
echo "$VERSION"
