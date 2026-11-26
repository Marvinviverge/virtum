#!/usr/bin/env bash
# Construit les fichiers de release : virtum-<version>-x86_64-linux.tar.gz, le paquet
# virtum_<version>_amd64.deb (scripts/package-deb.sh) et SHA256SUMS.
#
# Usage : scripts/package-release.sh <binaire virtum> <dossier de sortie>
# Contenu : binaire, LICENSE, README, SECURITY, CHANGELOG, scripts d'installation de l'hôte,
# exemples suivis par Git. Archive reproductible : fichiers triés, propriétaire 0,
# date du dernier commit.
set -euo pipefail

BIN="${1:?binaire virtum attendu}"
OUT="${2:?dossier de sortie attendu}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
BIN_VERSION="$("$BIN" --version | awk '{print $2}')"
if [[ "$BIN_VERSION" != "$VERSION" ]]; then
  echo "Le binaire annonce $BIN_VERSION, Cargo.toml $VERSION." >&2
  exit 1
fi

NAME="virtum-${VERSION}-x86_64-linux"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -r -- "$STAGE"' EXIT
DIR="$STAGE/$NAME"
mkdir -p "$DIR/scripts"

install -m 755 "$BIN" "$DIR/virtum"
install -m 644 LICENSE README.md README.fr.md SECURITY.md CHANGELOG.md CONTRIBUTING.md CONTRIBUTING.en.md "$DIR/"
install -m 755 scripts/setup-host.sh scripts/setup-var-lib.sh scripts/setup-apparmor-virtiofs.sh "$DIR/scripts/"
git ls-files -z examples | while IFS= read -r -d '' f; do
  install -D -m 644 "$f" "$DIR/$f"
done

EPOCH="$(git log -1 --format=%ct)"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" \
  -C "$STAGE" -cf - "$NAME" | gzip -n -9 > "$OUT/$NAME.tar.gz"

echo "OK — $OUT/$NAME.tar.gz"

"$ROOT/scripts/package-deb.sh" "$BIN" "$OUT"

(cd "$OUT" && sha256sum "$NAME.tar.gz" "virtum_${VERSION}_amd64.deb" > SHA256SUMS)
cat "$OUT/SHA256SUMS"
