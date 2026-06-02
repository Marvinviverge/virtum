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
chmod -R u=rwX,g=rwX,o= "$ROOT"

LEGACY_IMAGE="/home/$USER_NAME/.virtum/images/debian-12.qcow2"
if [[ -f "$LEGACY_IMAGE" && ! -f "$ROOT/images/debian-12.qcow2" ]]; then
  echo "Copie de l'image depuis $LEGACY_IMAGE …"
  cp -n "$LEGACY_IMAGE" "$ROOT/images/debian-12.qcow2"
  chown "$USER_NAME:$GROUP" "$ROOT/images/debian-12.qcow2"
  chmod 664 "$ROOT/images/debian-12.qcow2"
fi

echo "OK — Virtum utilisera $ROOT (préféré à ~/.virtum)."
echo "Relancez : virtum doctor && virtum run --os debian-12"
