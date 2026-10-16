#!/bin/sh
# Remove the cardio binary. cardio installs exactly ONE file (a static
# binary in ~/.local/bin or /usr/local/bin) and nothing else — no config,
# no cache, no daemon — so removal is complete.
# Usage: curl -fsSL https://raw.githubusercontent.com/Red-CF/log-cardio/main/uninstall.sh | sh
set -e

# the two prefixes install.sh itself uses, then whatever PATH has
for p in "$HOME/.local/bin/cardio" /usr/local/bin/cardio; do
  [ -e "$p" ] && { rm "$p"; echo "removed: $p"; exit 0; }
done
FOUND=$(command -v cardio 2>/dev/null || true)
if [ -n "$FOUND" ]; then
  rm "$FOUND"
  echo "removed: $FOUND"
  echo "note: it was outside the standard prefixes (${HOME}/.local/bin, /usr/local/bin)."
else
  echo "cardio not found — nothing to uninstall."
fi