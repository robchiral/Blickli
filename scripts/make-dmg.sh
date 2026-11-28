#!/bin/sh
# Build Blickli.app and pack it into a .dmg.
set -eu

cd "$(dirname "$0")/.."
./scripts/make-app.sh
DMG=$(./scripts/pack-dmg.sh)
echo "built $DMG"
