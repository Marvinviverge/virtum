#!/usr/bin/env bash
# Smoke lab hybride v0.3 (STEPS3 §7, étape 43) — web bridge+lab, db lab seul.
# Nécessite un hôte où macvtap bridge fonctionne (Ethernet recommandé ; Wi‑Fi / partage souvent KO).
#
# Usage :
#   VIRTUM_BRIDGE_SMOKE=1 ./scripts/smoke-test-lab-bridge.sh
#   VIRTUM_BRIDGE_SMOKE=1 VIRTUM_BIN=./target/release/virtum ./scripts/smoke-test-lab-bridge.sh
set -euo pipefail

if [[ "${VIRTUM_BRIDGE_SMOKE:-}" != "1" ]]; then
  echo "Smoke bridge ignoré (définir VIRTUM_BRIDGE_SMOKE=1 pour lancer ; réseau LAN physique requis)." >&2
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VIRTUM="${VIRTUM_BIN:-$ROOT/target/release/virtum}"
VIRTUMFILE="${VIRTUMFILE:-$ROOT/examples/Virtumfile.bridge-mixed}"

if [[ ! -x "$VIRTUM" ]]; then
  echo "Binaire introuvable: $VIRTUM (lancez: make release)" >&2
  exit 1
fi

echo "== virtum doctor (bridge) =="
"$VIRTUM" doctor

echo "== virtum plan -f $VIRTUMFILE =="
"$VIRTUM" plan -f "$VIRTUMFILE"

echo "== virtum pull debian-12 =="
"$VIRTUM" pull debian-12

echo "== virtum up -f $VIRTUMFILE =="
"$VIRTUM" up -f "$VIRTUMFILE"

echo "== virtum ps =="
"$VIRTUM" ps

# Colonnes : NAME LAB VM_ID OS STATUS DISK IP LAN FORWARD
WEB_IP="$("$VIRTUM" ps | awk '$1 == "web" { print $7; exit }')"
DB_IP="$("$VIRTUM" ps | awk '$1 == "db" { print $7; exit }')"
WEB_LAN="$("$VIRTUM" ps | awk '$1 == "web" { print $8; exit }')"

if [[ -z "$WEB_IP" || -z "$DB_IP" || "$WEB_IP" == "-" ]]; then
  echo "IP lab introuvable pour web/db (SSH / ping inter-VM)." >&2
  exit 1
fi

STATE_ROOT="${VIRTUM_STATE_DIR:-/var/lib/virtum}"
KEY="$STATE_ROOT/keys/virtum_id_ed25519"
SSH_BASE=(-i "$KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)

echo "== ping web → db (réseau lab, IP $DB_IP) =="
ssh "${SSH_BASE[@]}" "virtum@${WEB_IP}" ping -c1 -W3 "$DB_IP"

if [[ -n "$WEB_LAN" && "$WEB_LAN" != "-" ]]; then
  echo "== IP LAN web détectée : $WEB_LAN =="
else
  echo "== Avertissement : pas d'IP LAN pour web (souvent normal sur Wi‑Fi / partage téléphone) =="
fi

echo "== virtum down -f $VIRTUMFILE --destroy =="
"$VIRTUM" down -f "$VIRTUMFILE" --destroy

echo "OK — smoke lab bridge-mixed terminé."
