#!/usr/bin/env bash
# Parcours lab NAT de bout en bout (examples/Virtumfile) — KVM, libvirt, socat.
# Port HTTP : aligné sur examples/Virtumfile (18098 par défaut).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VIRTUM="${VIRTUM_BIN:-$ROOT/target/release/virtum}"
VIRTUMFILE="${VIRTUMFILE:-$ROOT/examples/Virtumfile}"

if [[ ! -x "$VIRTUM" ]]; then
  echo "Binaire introuvable: $VIRTUM (lancez: make release)" >&2
  exit 1
fi

echo "== virtum doctor =="
"$VIRTUM" doctor

echo "== virtum pull debian-13 (idempotent) =="
"$VIRTUM" pull debian-13

echo "== virtum up -f $VIRTUMFILE =="
"$VIRTUM" up -f "$VIRTUMFILE"

echo "== virtum ps =="
"$VIRTUM" ps

echo "== ping inter-VM (même lab, réseau virtum-lab-*) =="
WEB_IP="$("$VIRTUM" ps | awk '$1 == "web" { print $7; exit }')"
DB_IP="$("$VIRTUM" ps | awk '$1 == "db" { print $7; exit }')"
if [[ -z "$WEB_IP" || -z "$DB_IP" || "$WEB_IP" == "-" ]]; then
  echo "IPs lab introuvables dans virtum ps" >&2
  exit 1
fi
STATE_ROOT="${VIRTUM_STATE_DIR:-/var/lib/virtum}"
KEY="$STATE_ROOT/keys/virtum_id_ed25519"
# -F /dev/null : ignorer ~/.ssh/config (pas d'agent ni de redirection vers un invité).
SSH_BASE=(-F /dev/null -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ForwardAgent=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o GlobalKnownHostsFile=/dev/null)
ssh "${SSH_BASE[@]}" "virtum@${WEB_IP}" ping -c1 -W2 "$DB_IP"
ssh "${SSH_BASE[@]}" "virtum@${DB_IP}" ping -c1 -W2 "$WEB_IP"

HTTP_PORT="${LAB_HTTP_PORT:-18098}"
echo "== HTTP port forward (127.0.0.1:${HTTP_PORT}) =="
curl -sf --max-time 5 "http://127.0.0.1:${HTTP_PORT}/" | head -c 200
echo

echo "== virtum down -f $VIRTUMFILE =="
"$VIRTUM" down -f "$VIRTUMFILE"

echo "OK — smoke lab terminé (VM conservées ; --destroy pour tout supprimer)."
