#!/usr/bin/env bash
# Prérequis hôte Debian/Ubuntu pour Virtum (étape 24 STEPS2).
# Paquets KVM/libvirt, socat (ports lab), groupes libvirt/kvm.
set -euo pipefail

if [[ "${EUID:-}" -ne 0 ]]; then
  echo "Exécutez avec sudo : sudo $0" >&2
  exit 1
fi

TARGET_USER="${SUDO_USER:-${USER:-}}"
if [[ -z "$TARGET_USER" || "$TARGET_USER" == "root" ]]; then
  echo "Définissez SUDO_USER (lancez via sudo depuis un compte utilisateur)." >&2
  exit 1
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "Ce script cible Debian/Ubuntu (apt). Sur d'autres distributions," >&2
  echo "installez manuellement les paquets listés dans README.md." >&2
  exit 1
fi

PACKAGES=(
  qemu-kvm
  libvirt-daemon-system
  libvirt-clients
  qemu-utils
  cloud-image-utils
  openssh-client
  socat
)

echo "==> Mise à jour des index apt…"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq

echo "==> Installation des paquets Virtum…"
apt-get install -y -qq "${PACKAGES[@]}"

echo "==> Service libvirt…"
systemctl enable --now libvirtd 2>/dev/null || systemctl enable --now libvirt-daemon 2>/dev/null || true

if command -v virsh >/dev/null 2>&1; then
  if virsh net-info default >/dev/null 2>&1; then
    if ! virsh net-info default 2>/dev/null | grep -qE '^[[:space:]]*Active:[[:space:]]*yes'; then
      echo "==> Démarrage du réseau libvirt « default »…"
      virsh net-start default 2>/dev/null || true
    fi
  fi
fi

echo "==> Groupes libvirt et kvm pour ${TARGET_USER}…"
usermod -aG libvirt,kvm "$TARGET_USER"

echo ""
echo "OK — prérequis hôte installés pour ${TARGET_USER}."
echo ""
echo "Étapes suivantes (en tant que ${TARGET_USER}, après reconnexion ou newgrp) :"
echo "  1. Vérifiez les groupes : groups    # doit contenir libvirt et kvm"
echo "  2. Installez Virtum : ./scripts/install.sh   (ou make install)"
echo "  3. Stockage système : sudo ./scripts/setup-var-lib.sh"
echo "  4. Contrôle : virtum doctor"
echo ""
echo "Important : déconnectez-vous puis reconnectez-vous (ou : newgrp libvirt)"
echo "pour que les groupes libvirt/kvm soient pris en compte."
