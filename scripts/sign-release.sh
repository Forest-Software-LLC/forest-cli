#!/usr/bin/env sh
# Sign a published release manifest with an offline release key and upload the
# detached signature. Until this runs, new CLIs refuse to install the release —
# CI alone cannot mint an installable release.
#
# Usage:
#   scripts/sign-release.sh <tag> [key-file]
#
# Env:
#   FOREST_RELEASE_BUCKET  R2 bucket name (required; same as the workflow's R2_BUCKET var)
#   FOREST_INSTALL_BASE    release host override (default https://releases.forest.dev)
#
# Requires: ssh-keygen (with FIDO support if the key is hardware-backed),
# curl, and a wrangler login with write access to the bucket.
set -eu

TAG="${1:?usage: sign-release.sh <tag> [key-file]}"
KEY="${2:-$HOME/.ssh/forest_release_sk}"
BASE="${FOREST_INSTALL_BASE:-https://releases.forest.dev}"
BUCKET="${FOREST_RELEASE_BUCKET:?set FOREST_RELEASE_BUCKET to the R2 bucket name}"
NAMESPACE="forest-release"

[ -f "$KEY" ] || { echo "signing key not found: $KEY" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Sign the tag manifest's exact bytes. The latest/ copy is byte-identical
# (release.ts writes the same JSON to both paths), so one signature serves
# both — verified here rather than assumed, in case an older tag is re-signed
# after a newer release already moved latest/.
curl -fsSL "$BASE/cli/$TAG/latest.json" -o "$TMP/manifest.json"
curl -fsSL "$BASE/cli/latest/latest.json" -o "$TMP/latest-copy.json"
if ! cmp -s "$TMP/manifest.json" "$TMP/latest-copy.json"; then
    echo "cli/$TAG/latest.json and cli/latest/latest.json differ — $TAG is not the current latest release." >&2
    echo "Only the newest tag's signature may be published to latest/. Aborting." >&2
    exit 1
fi

echo "Signing cli/$TAG/latest.json — touch your security key when it blinks..."
ssh-keygen -Y sign -n "$NAMESPACE" -f "$KEY" "$TMP/manifest.json"

npx wrangler r2 object put "$BUCKET/cli/$TAG/latest.json.sig" \
    --file "$TMP/manifest.json.sig" --content-type text/plain --remote
npx wrangler r2 object put "$BUCKET/cli/latest/latest.json.sig" \
    --file "$TMP/manifest.json.sig" --content-type text/plain --remote

echo "Signature uploaded. New CLIs will now accept release $TAG."
