#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$root/scripts/personal-signing.sh"
unset HEX_SIGN_IDENTITY
original='Apple Development: Original Person (ORIGINAL01)'
other='Apple Development: Another Person (DIFFERENT2)'
fixture_authority=$original
fixture_identities=$(printf '  1) BBB "%s"\n  2) AAA "%s"\n' "$other" "$original")
codesign() { printf 'Authority=%s\n' "$fixture_authority" >&2; }
security() { printf '%s\n' "$fixture_identities"; }

# The first valid Keychain identity must never replace the installed signer.
[ "$(hex_personal_codesign_identity /fixture/Hex.app)" = "$original" ]
fixture_identities=$(printf '  1) AAA "%s"\n  2) BBB "%s"\n' "$original" "$other")
[ "$(hex_personal_codesign_identity /fixture/Hex.app)" = "$original" ]

# Losing the original private key fails closed, even when another key is valid.
fixture_identities=$(printf '  1) BBB "%s"\n' "$other")
if hex_personal_codesign_identity /fixture/Hex.app >/dev/null 2>&1; then
  echo 'Selected a replacement for a missing signing identity.' >&2
  exit 1
fi

# The initial transition from an upstream release requires an explicit signer.
fixture_authority='Developer ID Application: Upstream (UPSTREAM01)'
if hex_personal_codesign_identity /fixture/Hex.app >/dev/null 2>&1; then
  echo 'Silently changed the upstream signing identity.' >&2
  exit 1
fi
HEX_SIGN_IDENTITY=$other
[ "$(hex_personal_codesign_identity /fixture/Hex.app)" = "$other" ]
HEX_SIGN_IDENTITY=$original
if hex_personal_codesign_identity /fixture/Hex.app >/dev/null 2>&1; then
  echo 'Accepted an unavailable explicit signing identity.' >&2
  exit 1
fi
printf '%s\n' 'Personal signing identity guards passed.'
