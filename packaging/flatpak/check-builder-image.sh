#!/usr/bin/env bash
# Resolves the Flatpak builder image's digest and fails unless it is the one
# recorded in packaging/flatpak/builder-image.digest. The image is named in
# packaging/flatpak/builder-image.ref (a mutable tag: it is only how the digest
# is found; the release builds the digest). Used by the Release workflow's
# `flatpak-image` job (dry runs and tag releases alike) and by the Tag release
# workflow, so a moved tag is found before a tag exists.
#   packaging/flatpak/check-builder-image.sh [github-output-file]
# Prints "Resolved:" and "Recorded:" lines; with an output file, appends `digest=<resolved>`.
# Needs `docker buildx` (on GitHub's runners).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
image="$(tr -d '[:space:]' < "$here/builder-image.ref")"
want="$(tr -d '[:space:]' < "$here/builder-image.digest")"
got="$(docker buildx imagetools inspect "$image" | sed -n 's/^Digest: *//p' | sed -n 1p)"
if ! printf '%s' "$got" | grep -Eq '^sha256:[0-9a-f]{64}$'; then
  echo "::error::Couldn't resolve a digest for $image (got: $got)" >&2
  exit 1
fi
if ! printf '%s' "$want" | grep -Eq '^sha256:[0-9a-f]{64}$'; then
  echo "::error::packaging/flatpak/builder-image.digest does not hold a digest (got: $want). To record: echo $got > packaging/flatpak/builder-image.digest" >&2
  exit 1
fi
echo "Resolved: $image = $got"
echo "Recorded: $want"
[ -z "${1:-}" ] || echo "digest=$got" >> "$1"
if [ "$got" != "$want" ]; then
  echo "::error::The builder image is $got, not the recorded $want. Check that the tag is the one flathub-infra/actions-images built, then record it: echo $got > packaging/flatpak/builder-image.digest" >&2
  exit 1
fi
echo "The builder image is the recorded one."
