#!/usr/bin/env bash
# Prépare /var/lib/virtum pour libvirt système (évite AppArmor sur $HOME/.virtum).
set -euo pipefail

if [[ "${EUID:-}" -ne 0 ]]; then
  echo "Exécutez avec sudo : sudo $0" >&2
  exit 1
fi

USER_NAME="${SUDO_USER:-${USER:-root}}"
GROUP="libvirt"
ROOT="/var/lib/virtum"

mkdir -p "$ROOT/images" "$ROOT/instances" "$ROOT/keys"
chown -R "$USER_NAME:$GROUP" "$ROOT"
# Répertoires partagés avec le groupe libvirt : sticky bit, pour qu'un membre du groupe ne
# puisse ni renommer ni remplacer les fichiers de l'utilisateur (images, état).
# Pas de chmod récursif : les répertoires d'instance gardent leurs droits restreints.
chmod 1770 "$ROOT" "$ROOT/images" "$ROOT/instances"
# Clés SSH : propriétaire seul (QEMU n'en a pas besoin).
chmod 700 "$ROOT/keys"
find "$ROOT/keys" -type f -name '*.pub' -exec chmod 644 {} +
find "$ROOT/keys" -type f ! -name '*.pub' -exec chmod 600 {} +

LEGACY_IMAGE="/home/$USER_NAME/.virtum/images/debian-12.qcow2"
if [[ -f "$LEGACY_IMAGE" && ! -f "$ROOT/images/debian-12.qcow2" ]]; then
  echo "Copie de l'image depuis $LEGACY_IMAGE …"
  cp -n "$LEGACY_IMAGE" "$ROOT/images/debian-12.qcow2"
  chown "$USER_NAME:$GROUP" "$ROOT/images/debian-12.qcow2"
  chmod 444 "$ROOT/images/debian-12.qcow2"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -x "$SCRIPT_DIR/setup-apparmor-virtiofs.sh" ]]; then
  "$SCRIPT_DIR/setup-apparmor-virtiofs.sh"
fi

echo "OK — Virtum utilisera $ROOT (préféré à ~/.virtum)."
echo "Relancez : virtum doctor && virtum run --os debian-13"
