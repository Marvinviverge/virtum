#!/usr/bin/env bash
# Installe la commande `virtum` dans le PATH (~/.cargo/bin ou ~/.local/bin).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

INSTALL_DIR="${VIRTUM_INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"

echo "==> Compilation (release)…"
cargo build --release

mkdir -p "$INSTALL_DIR"
install -m 755 target/release/virtum "$INSTALL_DIR/virtum"

if command -v virtum >/dev/null 2>&1; then
  echo "OK — virtum disponible : $(command -v virtum)"
  virtum --version 2>/dev/null || true
else
  echo "OK — binaire installé dans $INSTALL_DIR/virtum"
  echo "Ajoutez ce répertoire à votre PATH, par exemple dans ~/.bashrc :"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi
