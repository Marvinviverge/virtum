#!/usr/bin/env bash
# Validation manuelle du cycle de vie d'une VM : run, shell, stop, start, destroy.
# Prérequis : KVM, libvirt système (réseau default), image debian-13 en cache recommandée.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -n "${VIRTUM_STATE_DIR:-}" ]]; then
  VIRTUM_ROOT="$VIRTUM_STATE_DIR"
elif [[ -d /var/lib/virtum ]]; then
  VIRTUM_ROOT="/var/lib/virtum"
else
  VIRTUM_ROOT="${HOME}/.virtum"
fi

if [[ -x "$ROOT/target/release/virtum" ]]; then
  VIRTUM="$ROOT/target/release/virtum"
elif [[ -x "$ROOT/target/debug/virtum" ]]; then
  VIRTUM="$ROOT/target/debug/virtum"
else
  echo "Binaire virtum introuvable. Lancez : cargo build --release" >&2
  exit 1
fi

echo "==> virtum doctor"
"$VIRTUM" doctor

echo ""
echo "==> Cycle de vie (run → stop → start → destroy)"
echo "    Ce script lance run ; complétez shell manuellement si besoin."
RUN_LOG="$(mktemp)"
trap 'rm -f "$RUN_LOG"' EXIT
"$VIRTUM" run --os debian-13 2>&1 | tee /dev/stderr | tee "$RUN_LOG" >/dev/null
VM_ID="$(awk '/^VM démarrée: / { print $3 }' "$RUN_LOG")"
IP="$(awk '/^  IP: / { print $2 }' "$RUN_LOG")"
if [[ -z "${VM_ID:-}" ]]; then
  echo "Impossible d'extraire le vm-id depuis la sortie de virtum run." >&2
  exit 1
fi
if [[ -z "${IP:-}" ]]; then
  echo "Impossible d'extraire l'IP depuis la sortie de virtum run." >&2
  exit 1
fi
echo "    vm-id = $VM_ID"
echo "    ip    = $IP (réseau libvirt default)"

echo ""
echo "==> Vérifications intermédiaires"
virsh domstate "$VM_ID" | grep -q running
[[ "$IP" == 192.168.122.* ]] || echo "    (avertissement: IP hors plage 192.168.122.x : $IP)"
test -d "$VIRTUM_ROOT/instances/$VM_ID"
grep -q "\"id\": \"$VM_ID\"" "$VIRTUM_ROOT/metadata.json"

echo ""
echo "    Connectez-vous : $VIRTUM shell $VM_ID"
echo "    Dans la VM : whoami && hostname && sudo true"
echo "    Puis appuyez sur Entrée ici pour stop → start → destroy…"
read -r _

echo "==> virtum stop $VM_ID"
"$VIRTUM" stop "$VM_ID"
virsh domstate "$VM_ID" 2>/dev/null | grep -qE 'shut off|inactive' || true

echo "==> virtum ps -a (VM arrêtée)"
"$VIRTUM" ps -a | grep -q "$VM_ID"

echo "==> virtum start $VM_ID"
START_LOG="$(mktemp)"
trap 'rm -f "$RUN_LOG" "$START_LOG"' EXIT
"$VIRTUM" start "$VM_ID" 2>&1 | tee /dev/stderr | tee "$START_LOG" >/dev/null
IP="$(awk '/^  IP: / { print $2 }' "$START_LOG")"
virsh domstate "$VM_ID" | grep -q running
[[ -n "${IP:-}" ]] || { echo "IP manquante après virtum start" >&2; exit 1; }

echo "==> virtum destroy $VM_ID"
"$VIRTUM" destroy "$VM_ID"

echo "==> Vérifications post-destroy"
if virsh list --all 2>/dev/null | grep -q "$VM_ID"; then
  echo "ERREUR: domaine $VM_ID encore présent dans virsh list --all" >&2
  exit 1
fi
if [[ -d "$VIRTUM_ROOT/instances/$VM_ID" ]]; then
  echo "ERREUR: répertoire instance encore présent" >&2
  exit 1
fi
if grep -q "\"id\": \"$VM_ID\"" "$VIRTUM_ROOT/metadata.json" 2>/dev/null; then
  echo "ERREUR: entrée metadata encore présente" >&2
  exit 1
fi
# Image datée (debian-13-<date>.qcow2) : conservée après destroy.
compgen -G "$VIRTUM_ROOT/images/debian-13-*.qcow2" >/dev/null

echo ""
echo "OK — parcours MVP validé pour $VM_ID"
