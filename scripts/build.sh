#!/usr/bin/env bash
#
# Build, code-sign, and (re)install trove-collector.
#
# Why sign? macOS TCC (Screen Recording) keys a grant to the binary's
# *designated requirement*. Cargo's default ad-hoc signature makes that
# requirement a bare cdhash, which changes on every build — so every rebuild
# would revoke the grant. Signing with a stable identity makes the requirement
# identity-based, so the grant survives rebuilds. Grant the permission ONCE.
#
# Usage:
#   scripts/build.sh                 # build + sign + (re)start the launch agent
#   scripts/build.sh --no-install    # build + sign only
#
# Identity: TROVE_SIGN_ID (SHA-1 from `security find-identity -v -p codesigning`).
# If unset, the first "Apple Development" identity in the keychain is used.
set -euo pipefail
cd "$(dirname "$0")/.."

SIGN_ID="${TROVE_SIGN_ID:-${TROVED_SIGN_ID:-}}"
if [[ -z "$SIGN_ID" ]]; then
  SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
    | grep 'Apple Development' | head -1 | awk '{print $2}')"
  if [[ -z "$SIGN_ID" ]]; then
    echo "No Apple Development signing identity found and TROVE_SIGN_ID is unset." >&2
    echo "List yours with: security find-identity -v -p codesigning" >&2
    exit 1
  fi
  echo "==> using signing identity $SIGN_ID (set TROVE_SIGN_ID to pin one)"
fi
IDENTIFIER="com.davidwills.trove-collector"
BIN="target/release/trove-collector"

echo "==> building trove-collector (release)"
source "$HOME/.cargo/env" 2>/dev/null || true
cargo build --release

echo "==> code-signing $BIN with $IDENTIFIER"
codesign --force --sign "$SIGN_ID" --identifier "$IDENTIFIER" "$BIN"
codesign -d -r- "$BIN" 2>&1 | grep -q "certificate leaf" \
  && echo "    OK — identity-based requirement (TCC grant survives rebuilds)" \
  || { echo "    WARNING: requirement is not identity-based; check the signing identity"; exit 1; }

if [[ "${1:-}" != "--no-install" ]]; then
  echo "==> (re)installing launch agent (restarts on the new build)"
  "$BIN" install
fi

echo "==> done"
