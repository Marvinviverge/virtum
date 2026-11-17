# Virtum — raccourcis build / install (complète scripts/install.sh)

CARGO ?= cargo
TARGET_DIR ?= target
RELEASE_BIN := $(TARGET_DIR)/release/virtum
DEBUG_BIN := $(TARGET_DIR)/debug/virtum
INSTALL_DIR ?= $(HOME)/.cargo/bin
INSTALLED_BIN := $(INSTALL_DIR)/virtum

.PHONY: all release build debug install test fmt clippy check clean help setup-host setup-var-lib smoke-lab smoke-lab-bridge audit

all: release

## Compile le binaire optimisé (équivalent: cargo build --release)
release:
	$(CARGO) build --release

build: debug

debug:
	$(CARGO) build

## Compile release + installe dans $(INSTALL_DIR)/virtum
install: release
	@mkdir -p "$(INSTALL_DIR)"
	install -m 755 "$(RELEASE_BIN)" "$(INSTALLED_BIN)"
	@echo "OK — $(INSTALLED_BIN)"
	@command -v virtum >/dev/null 2>&1 && virtum --version 2>/dev/null || true

test:
	@# TMPDIR privé (mktemp -d : 0700, nom aléatoire) : les tests n'écrivent jamais dans
	@# un /tmp partagé où un autre compte aurait pu planter des liens symboliques.
	TMPDIR=$$(mktemp -d) $(CARGO) test

fmt:
	$(CARGO) fmt

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

## cargo audit (ignores via .cargo/audit.toml, lu automatiquement) — warn si binaire absent
audit:
	@if command -v cargo-audit >/dev/null 2>&1; then \
		cargo audit; \
	else \
		echo "Avertissement: cargo-audit absent — installez-le pour le scan RustSec (cargo install cargo-audit)."; \
	fi

## fmt + clippy + tests + audit (si cargo-audit installé)
check: fmt clippy test audit

## Smoke lab NAT (examples/Virtumfile) — nécessite make release + images
smoke-lab: release
	@test -f scripts/smoke-test-lab.sh || { echo "scripts/smoke-test-lab.sh introuvable"; exit 1; }
	bash scripts/smoke-test-lab.sh

## Smoke lab hybride bridge — VIRTUMFILE=… (web en bridge + db), Ethernet recommandé
smoke-lab-bridge: release
	@test -f scripts/smoke-test-lab-bridge.sh || { echo "scripts/smoke-test-lab-bridge.sh introuvable"; exit 1; }
	VIRTUM_BRIDGE_SMOKE=1 bash scripts/smoke-test-lab-bridge.sh

clean:
	$(CARGO) clean

## Prérequis hôte apt + groupes libvirt/kvm (sudo, Debian/Ubuntu)
setup-host:
	@test -x scripts/setup-host.sh || { echo "scripts/setup-host.sh introuvable"; exit 1; }
	sudo ./scripts/setup-host.sh

## Prépare /var/lib/virtum (sudo)
setup-var-lib:
	@test -x scripts/setup-var-lib.sh || { echo "scripts/setup-var-lib.sh introuvable"; exit 1; }
	sudo ./scripts/setup-var-lib.sh

help:
	@echo "Cibles Virtum :"
	@echo "  make release   — binaire dans $(RELEASE_BIN)"
	@echo "  make install   — release + copie vers $(INSTALL_DIR)"
	@echo "  make debug     — build dev ($(DEBUG_BIN))"
	@echo "  make test      — cargo test"
	@echo "  make check     — fmt + clippy + test + audit (si cargo-audit installé)"
	@echo "  make clean     — cargo clean"
	@echo "  make setup-host    — sudo scripts/setup-host.sh (apt, libvirt, socat)"
	@echo "  make setup-var-lib — sudo scripts/setup-var-lib.sh"
	@echo "  make smoke-lab         — scripts/smoke-test-lab.sh (NAT)"
	@echo "  make smoke-lab-bridge — hybride (VIRTUM_BRIDGE_SMOKE=1)"
