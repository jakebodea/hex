#!/bin/sh
# Preserve the installed personal app's signer: macOS privacy grants bind to it.
hex_personal_codesign_identity() {
  base=$1
  identity=${HEX_SIGN_IDENTITY:-}
  if [ -z "$identity" ]; then
    identity=$(codesign -dv --verbose=4 "$base" 2>&1 | sed -n 's/^Authority=//p' | head -1)
    case "$identity" in
      'Apple Development: '*) ;;
      *)
        echo "Base app is not Apple Development signed. Set HEX_SIGN_IDENTITY explicitly for the initial personal build." >&2
        return 1
        ;;
    esac
  fi
  if ! security find-identity -v -p codesigning | grep -Fq "\"$identity\""; then
    echo "The app's signing identity is unavailable: $identity. Refusing to select another certificate and invalidate privacy grants." >&2
    return 1
  fi
  printf '%s\n' "$identity"
}
