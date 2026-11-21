#!/usr/bin/env bash
# Construit le paquet Debian : virtum_<version>_amd64.deb.
#
# Usage : scripts/package-deb.sh <binaire virtum> <dossier de sortie>
# Le paquet installe le binaire, la documentation et les scripts de préparation de l'hôte.
# apt installe libvirt et QEMU comme dépendances. virtiofsd et bubblewrap (dossiers partagés)
# sont seulement recommandés : virtiofsd n'existe pas en paquet séparé sur Ubuntu 22.04 ni
# Debian 12. Aucun script de maintenance : l'ajout aux groupes libvirt/kvm et la préparation
# de /var/lib/virtum restent des étapes explicites.
# Paquet reproductible : fichiers datés du dernier commit (SOURCE_DATE_EPOCH).
set -euo pipefail

BIN="${1:?binaire virtum attendu}"
OUT="${2:?dossier de sortie attendu}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
EPOCH="$(git log -1 --format=%ct)"
export SOURCE_DATE_EPOCH="$EPOCH"
DATE_RFC2822="$(LC_ALL=C date -u -d "@$EPOCH" -R)"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -r -- "$STAGE"' EXIT
PKG="$STAGE/virtum"
DOC="$PKG/usr/share/doc/virtum"
umask 022

install -D -m 755 "$BIN" "$PKG/usr/bin/virtum"
install -D -m 755 scripts/setup-var-lib.sh "$PKG/usr/share/virtum/setup-var-lib.sh"
install -D -m 755 scripts/setup-apparmor-virtiofs.sh "$PKG/usr/share/virtum/setup-apparmor-virtiofs.sh"
install -D -m 644 README.md "$DOC/README.md"
install -m 644 README.en.md SECURITY.md "$DOC/"
install -D -m 644 examples/Virtumfile "$DOC/examples/Virtumfile"

cat > "$DOC/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: virtum
Upstream-Contact: Virtum <contact@virtum.dev>
Source: https://github.com/Marvinviverge/virtum

Files: *
Copyright: $(date -u -d "@$EPOCH" +%Y) Marvinviverge
License: Apache-2.0

License: Apache-2.0
 On Debian systems, the full text of the Apache License, Version 2.0 can be
 found in the file /usr/share/common-licenses/Apache-2.0.
EOF

# Paquet natif : changelog au format Debian, renvoyant au CHANGELOG du projet.
{
  echo "virtum ($VERSION) stable; urgency=medium"
  echo
  echo "  * Version $VERSION. Détail des changements :"
  echo "    https://github.com/Marvinviverge/virtum/blob/main/CHANGELOG.md"
  echo
  echo " -- Virtum <contact@virtum.dev>  $DATE_RFC2822"
} | gzip -n -9 > "$DOC/changelog.gz"
chmod 644 "$DOC/changelog.gz"

SIZE_KB="$(du -sk --exclude=DEBIAN "$PKG" | cut -f1)"
mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: virtum
Version: $VERSION
Architecture: amd64
Maintainer: Virtum <contact@virtum.dev>
Installed-Size: $SIZE_KB
Depends: libc6 (>= 2.34), libgcc-s1, libvirt-daemon-system, libvirt-clients, qemu-system-x86, qemu-utils, cloud-image-utils, openssh-client, socat
Recommends: virtiofsd, bubblewrap
Suggests: ansible-core | ansible, ufw
Section: admin
Priority: optional
Homepage: https://virtum.dev
Description: local KVM VMs and labs described in a Virtumfile
 Virtum runs local virtual machines and multi-VM labs with a Docker-like
 experience (run, ps, shell, up, down), on KVM and libvirt.
 .
 After installing: add your account to the libvirt and kvm groups
 (sudo usermod -aG libvirt,kvm \$USER), log in again, prepare the storage
 (sudo /usr/share/virtum/setup-var-lib.sh), then run virtum doctor.
EOF
(cd "$PKG" && find usr -type f -print0 | LC_ALL=C sort -z | xargs -0 md5sum > DEBIAN/md5sums)
chmod 644 "$PKG/DEBIAN/md5sums"
find "$PKG" -exec touch -h -d "@$EPOCH" {} +

DEB="virtum_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$PKG" "$OUT/$DEB" >/dev/null
echo "OK — $OUT/$DEB"
