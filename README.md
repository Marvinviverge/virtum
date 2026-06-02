# Virtum

**Virtum : la VM, tout simplement.**

CLI Linux pour lancer des VM locales reproductibles avec une expérience proche de Docker : `run`, `ps`, `shell`, `stop`, `start`, `destroy`.

## Statut

**MVP v0.1 terminé** — cycle `doctor` → `run` → `ps` → `shell` → `stop` → `start` → `destroy` (voir [`STEPS.md`](./STEPS.md)).

**v0.2 terminé (lab-as-code)** — Virtumfile, `virtum up` / `down`, `pull`, `disk-size`, ports (`socat`), provision, réseau isolé **`virtum-lab-<name>`** : [`STEPS2.md`](./STEPS2.md), [`SPEC_TECH_V0.2.md`](./SPEC_TECH_V0.2.md). Exemple : [`examples/Virtumfile`](./examples/Virtumfile).

**Roadmap v0.3** (réseau `bridge` lab / machine, NAT par défaut) : [`STEPS3.md`](./STEPS3.md), spec réseau [`SPEC_TECH_V0.3.md`](./SPEC_TECH_V0.3.md) (brouillon).

## Documentation

| Document | Rôle |
| :--- | :--- |
| [`PITCH.md`](./PITCH.md) | Vision produit et positionnement |
| [`PRD.md`](./PRD.md) | Exigences produit et roadmap long terme |
| [`SPEC_TECH_V0.1.md`](./SPEC_TECH_V0.1.md) | **Contrat technique du MVP** |
| [`STEPS.md`](./STEPS.md) | Plan d’implémentation **v0.1** (terminé) |
| [`STEPS2.md`](./STEPS2.md) | Plan d’implémentation **v0.2** |
| [`SPEC_TECH_V0.2.md`](./SPEC_TECH_V0.2.md) | Contrat technique **v0.2** (lab, réseau, ports) |

Le périmètre exécutable du MVP est défini par **SPEC + STEPS**, pas par l’intégralité du PRD.

## Prérequis hôte (Linux)

- **Rust** (toolchain stable + `cargo`) — [rustup.rs](https://rustup.rs) ; requis pour compiler via `install.sh`.
- Virtualisation **KVM** activée (BIOS/UEFI).

### Debian / Ubuntu (recommandé)

```bash
sudo ./scripts/setup-host.sh    # apt, libvirt, socat, groupes libvirt/kvm
# Reconnectez la session (ou : newgrp libvirt)
```

Équivalent : `make setup-host`. Le script installe notamment **socat** (mapping de ports du Virtumfile).

### Installation manuelle (autres distros ou sans script)

```bash
sudo apt update && sudo apt install -y \
  qemu-kvm libvirt-daemon-system libvirt-clients \
  qemu-utils cloud-image-utils openssh-client socat
sudo usermod -aG libvirt,kvm $USER
```

Puis **reconnectez la session** (ou `newgrp libvirt`).

## Installation (commande `virtum` dans le PATH)

Ordre recommandé depuis la racine du dépôt :

```bash
sudo ./scripts/setup-host.sh   # une fois par machine (si pas déjà fait)
./scripts/install.sh           # ou : make install
sudo ./scripts/setup-var-lib.sh
virtum doctor
```

| Script / cible | Rôle |
| :--- | :--- |
| `setup-host.sh` | Paquets apt, `libvirtd`, groupes `libvirt`/`kvm` |
| `install.sh` / `make install` | Compile et installe `virtum` dans `~/.cargo/bin` |
| `setup-var-lib.sh` | Prépare `/var/lib/virtum` (stockage QEMU système) |
| `virtum doctor` | Binaires, KVM, groupes, réseau `default`, stockage, socat |

Le répertoire `~/.cargo/bin` doit être dans votre `PATH` (souvent déjà le cas avec Rust).

Alternative :

```bash
sudo ./scripts/setup-host.sh
cargo install --path . --force
sudo ./scripts/setup-var-lib.sh
virtum doctor
```

Vérification :

```bash
which virtum
virtum doctor
```

Tous les checks `doctor` doivent être `[ok]` avant le premier `run`.

**Premier `virtum run --os debian-12`** : télécharge l’image cloud Debian (~400 Mo) puis attend cloud-init/SSH (1–2 min au premier boot).

## Build (sans installation)

```bash
make release
./target/release/virtum doctor
```

Sans `setup-var-lib.sh`, `doctor` échouera sur le stockage si Virtum utilise encore `~/.virtum`.

## Usage

```bash
virtum doctor
virtum pull debian-12        # télécharge l'image en cache (idempotent)
virtum pull ubuntu-24.04     # 2e OS (v0.2)
virtum run --os debian-12 --name db   # alias optionnel (virtum shell db)
virtum run --os debian-12 --disk-size 10G   # disque racine ~10G (cloud-init growpart)
virtum run --os debian-12
virtum up                    # crée ou relance le lab (Virtumfile) ; après `down`, relance sans recréer
virtum up -f examples/Virtumfile   # ex. ports 18080:80 → http://127.0.0.1:18080
virtum down                  # arrête toutes les VM du lab (Virtumfile `name`)
virtum down --destroy        # supprime le lab
virtum ps          # VM en cours (NAME, LAB, DISK, IP, …)
virtum ps -a       # toutes les VM connues (y compris arrêtées)
virtum shell <vm-id>
virtum stop <vm-id> [vm…]      # une ou plusieurs VM (id ou name)
virtum start <vm-id> [vm…]     # relancer une ou plusieurs VM arrêtées
virtum destroy <vm-id> [vm…]   # id/name ; orphelin libvirt (`virsh list` sans `virtum ps`)
```

Virtum utilise **libvirt système** (`qemu:///system`).

- **`virtum run`** (VM seule) : réseau libvirt **`default`** (`192.168.122.x`).
- **`virtum up`** (lab) : réseau dédié **`virtum-lab-<name>`** par lab (NAT isolé, ex. `192.168.64.0/24` selon le nom). Les machines du même lab se joignent entre elles ; les labs différents sont séparés. SSH et `ports:` depuis l’hôte restent possibles.

Après mise à jour réseau (étape 30), un lab déjà déployé sur `default` doit être recréé : `virtum down -f Virtumfile --destroy` puis `virtum up`.

Le stockage actif est **`/var/lib/virtum`** (images, instances, clés, `metadata.json`) — pas `~/.virtum`, après `setup-var-lib.sh`. `virtum doctor` doit afficher `[ok] Stockage Virtum`.

## Validation

### MVP v0.1

Checklist manuelle :

```bash
virtum doctor
virtum run --os debian-12 --name db   # alias optionnel (virtum shell db)
virtum run --os debian-12 --disk-size 10G   # disque racine ~10G (cloud-init growpart)
virtum run --os debian-12
virtum ps               # VM active + IP
virtum shell <vm-id>    # whoami → virtum ; hostname ; sudo true
virtum stop <vm-id>
virtum start <vm-id>    # relancer la même VM
virtum ps -a            # VM arrêtée visible
virtum destroy disk-test disk-test2   # plusieurs VM à la fois

virsh list --all | grep <vm-id>              # vide
ls /var/lib/virtum/instances/<vm-id>         # doit échouer
grep -q "<vm-id>" /var/lib/virtum/metadata.json && echo "ERREUR" || echo "OK metadata"
test -f /var/lib/virtum/images/debian-12.qcow2   # image de base intacte
```

Script interactif (arrêt après `run`, shell manuel, puis stop → start → destroy) :

```bash
chmod +x scripts/smoke-test.sh
./scripts/smoke-test.sh
```

### Lab v0.2

Parcours complet (après `make release`, lab déjà validé manuellement : ping inter-VM, `curl :18080`) :

```bash
chmod +x scripts/smoke-test-lab.sh
./scripts/smoke-test-lab.sh
# ping web↔db, curl :18080, puis down (ajoutez --destroy dans le script si besoin)
```

## Licence

À définir.
