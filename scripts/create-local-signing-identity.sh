#!/bin/sh
set -eu

IDENTITY="${VOXTYPE_SIGNING_IDENTITY:-VoxType Local Development}"
KEYCHAIN="${VOXTYPE_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

if security find-identity -v -p codesigning "$KEYCHAIN" | grep -F "\"$IDENTITY\"" >/dev/null; then
  echo "Code-signing identity already exists: $IDENTITY"
  exit 0
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/voxtype-signing.XXXXXX")"
KEY="$WORK/identity.key"
CERT="$WORK/identity.crt"
ARCHIVE="$WORK/identity.p12"
PASSWORD="$(openssl rand -hex 32)"

cleanup() {
  rm -f "$KEY" "$CERT" "$ARCHIVE"
  rmdir "$WORK"
}
trap cleanup EXIT INT TERM

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -sha256 \
  -days 3650 \
  -nodes \
  -subj "/CN=$IDENTITY/O=VoxType Local Development" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -keyout "$KEY" \
  -out "$CERT" \
  >/dev/null 2>&1

openssl pkcs12 \
  -export \
  -legacy \
  -name "$IDENTITY" \
  -inkey "$KEY" \
  -in "$CERT" \
  -out "$ARCHIVE" \
  -passout "pass:$PASSWORD"

security import "$ARCHIVE" \
  -k "$KEYCHAIN" \
  -P "$PASSWORD" \
  -T /usr/bin/codesign
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$CERT"

if ! security find-identity -v -p codesigning "$KEYCHAIN" | grep -F "\"$IDENTITY\"" >/dev/null; then
  echo "Could not create code-signing identity: $IDENTITY" >&2
  exit 1
fi

echo "Created code-signing identity: $IDENTITY"
