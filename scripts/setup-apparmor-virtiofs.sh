#!/usr/bin/env bash
# Autorise QEMU (libvirt-qemu) à se connecter aux sockets virtiofsd lancés par Virtum.
# Requis sur Ubuntu/Debian avec AppArmor + libvirt système (virtiofs externe).
set -euo pipefail

if [[ "${EUID:-}" -ne 0 ]]; then
  echo "Exécutez avec sudo : sudo $0" >&2
  exit 1
fi

SNIPPET_DIR="/etc/apparmor.d/abstractions/libvirt-qemu.d"
SNIPPET="${SNIPPET_DIR}/virtum-virtiofs"

mkdir -p "$SNIPPET_DIR"
cat >"$SNIPPET" <<'EOF'
# Virtum — sockets virtiofsd (lancés hors libvirt, chemins sous stockage Virtum)
  /var/lib/virtum/instances/*/virtiofs-*.sock rw,
  @{HOME}/.virtum/instances/*/virtiofs-*.sock rw,
EOF

if command -v apparmor_parser >/dev/null 2>&1; then
  apparmor_parser -r /etc/apparmor.d/usr.sbin.libvirtd 2>/dev/null || true
  apparmor_parser -r /etc/apparmor.d/libvirt/libvirt-* 2>/dev/null || true
fi

echo "OK — règle AppArmor Virtum virtiofs : $SNIPPET"
echo "Relancez : virtum up -f examples/Virtumfile.mounts"
