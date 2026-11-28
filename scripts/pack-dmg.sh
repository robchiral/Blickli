#!/bin/sh
# Pack target/Blickli.app into a versioned disk image.
set -eu

cd "$(dirname "$0")/.."
APP=target/Blickli.app
if [ ! -d "$APP" ]; then
  echo "$APP does not exist: run scripts/make-app.sh first" >&2
  exit 1
fi

VERSION=$(./scripts/version.sh)
DMG=target/Blickli-$VERSION.dmg

STAGE=$(mktemp -d target/blickli-dmg.XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
trap 'exit 1' HUP INT TERM
rm -f "$DMG"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

diskutil image create from --format UDZO --volumeName "Blickli $VERSION" "$STAGE" "$DMG" >&2
echo "$DMG"
