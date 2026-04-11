#!/bin/bash
set -euo pipefail
# Create a symlink from the service home media dir to the user's media path.
# The quadlet uses a fixed bind mount path; this script bridges it to the
# user-configured media directory.
MEDIA_LINK="$RYRA_SERVICE_HOME/media"
[ -e "$MEDIA_LINK" ] && exit 0
if [ -z "${JELLYFIN_MEDIA_DIR:-}" ]; then
  mkdir -p "$MEDIA_LINK"
  exit 0
fi
mkdir -p "$JELLYFIN_MEDIA_DIR"
ln -sf "$JELLYFIN_MEDIA_DIR" "$MEDIA_LINK"
