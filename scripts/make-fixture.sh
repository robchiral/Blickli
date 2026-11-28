#!/bin/sh
# Create an e2e test fixture with four HEIC images of known names, sizes, and timestamps.
#
#   sh scripts/make-fixture.sh [folder]     # default ~/blickli-e2e-fixture
#   BLICKLI_E2E=1 cargo run -p blickli --features e2e -- ~/blickli-e2e-fixture
set -eu

FIXTURE=${1:-$HOME/blickli-e2e-fixture}
# Resolve relative paths before cd.
case "$FIXTURE" in
  /*) ;;
  *) FIXTURE=$PWD/$FIXTURE ;;
esac
cd "$(dirname "$0")/.."

# Avoid Desktop (TCC blocks unsigned binaries) and /tmp (FSEvents watcher is unreliable).
case "$FIXTURE" in
  "$HOME/Desktop"/* | /tmp/* | /private/tmp/*)
    echo "Refusing $FIXTURE: choose a folder outside Desktop and /tmp" >&2
    echo "(macOS permissions and FSEvents break the trash and watcher tests there)." >&2
    exit 1
    ;;
esac

if ! mkdir "$FIXTURE"; then
  echo "Choose a new fixture folder in an existing directory." >&2
  exit 1
fi

ICONS=branding/macos/AppIcon.appiconset

# Name order: apple, img1, img2, img10. Sizes differ for the size sort.
for pair in apple:icon-512 img1:icon-256 img2:icon-128 img10:icon-512@2x; do
  name=${pair%%:*}
  icon=${pair##*:}
  sips -s format heic "$ICONS/$icon.png" --out "$FIXTURE/$name.heic" >/dev/null
done

# Modified dates follow name order.
touch -t 202601010101 "$FIXTURE/apple.heic"
touch -t 202602010101 "$FIXTURE/img1.heic"
touch -t 202603010101 "$FIXTURE/img2.heic"
touch -t 202604010101 "$FIXTURE/img10.heic"
touch "$FIXTURE/.blickli-fixture"

echo "built $FIXTURE"
ls "$FIXTURE"
