# Virtum

**Virtum : la VM, tout simplement.**

CLI Linux pour lancer des VM locales reproductibles avec une expérience proche de Docker : `run`, `ps`, `shell`, `stop`, `start`, `destroy`.

## Statut

**MVP v0.1 terminé** — cycle `doctor` → `run` → `ps` → `shell` → `stop` → `start` → `destroy` (voir [`STEPS.md`](./STEPS.md)).

**Roadmap v0.2** (Virtumfile, multi-VM, ports, `pull`, etc.) : [`STEPS2.md`](./STEPS2.md). Déjà disponible : `virtum up` / `virtum down` (Virtumfile).

## Documentation

| Document | Rôle |
| :--- | :--- |
| [`PITCH.md`](./PITCH.md) | Vision produit et positionnement |
| [`PRD.md`](./PRD.md) | Exigences produit et roadmap long terme |
| [`SPEC_TECH_V0.1.md`](./SPEC_TECH_V0.1.md) | **Contrat technique du MVP** |
| [`STEPS.md`](./STEPS.md) | Plan d’implémentation **v0.1** (terminé) |
| [`STEPS2.md`](./STEPS2.md) | Plan d’implémentation **v0.2** (roadmap tickets) |

Le périmètre exécutable du MVP est défini par **SPEC + STEPS**, pas par l’intégralité du PRD.

## Prérequis hôte (Linux)

- **Rust** (toolchain stable + `cargo`) — [rustup.rs](https://rustup.rs) ; requis pour compiler via `install.sh`.
- Virtualisation **KVM** activée (BIOS/UEFI) ; le service **libvirt** est en général démarré avec `libvirt-daemon-system`.

Paquets système :

```bash
sudo apt update && sudo apt install -y \
  qemu-kvm libvirt-daemon-system libvirt-clients \
  qemu-utils cloud-image-utils openssh-client
```

Ajoutez votre utilisateur aux groupes `libvirt` et `kvm`, puis **reconnectez la session** (ou `newgrp libvirt` dans le terminal courant) :

```bash
sudo usermod -aG libvirt,kvm $USER
```

## Installation (commande `virtum` dans le PATH)

Depuis la racine du dépôt, **après** les prérequis ci-dessus :

```bash
make install              # release + copie vers ~/.cargo/bin/virtum
sudo make setup-var-lib   # une fois
virtum doctor
```

Équivalent : `./scripts/install.sh`. Pour recompiler sans réinstaller : `make release` → `target/release/virtum`.

| Étape | Rôle |
| :--- | :--- |
| `install.sh` | Compile le binaire et l’installe dans `~/.cargo/bin/virtum` (ou `$VIRTUM_INSTALL_DIR`) |
| `setup-var-lib.sh` | Prépare `/var/lib/virtum` (stockage lisible par QEMU système, hors `$HOME`) |
| `virtum doctor` | Vérifie binaires, KVM, groupes, réseau `default`, stockage |

Le répertoire `~/.cargo/bin` doit être dans votre `PATH` (souvent déjà le cas avec Rust).

Alternative équivalente à `install.sh` :

```bash
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

## Usage (v0.1)

```bash
virtum doctor
virtum run --os debian-12 --name db   # alias optionnel (virtum shell db)
virtum run --os debian-12 --disk-size 10G   # disque racine ~10G (cloud-init growpart)
virtum run --os debian-12
virtum up                    # déploie ./Virtumfile (multi-VM, réseau default)
virtum up -f examples/Virtumfile
virtum down                  # arrête toutes les VM du lab (Virtumfile `name`)
virtum down --destroy        # supprime le lab
virtum ps          # VM en cours (NAME, LAB, DISK, IP, …)
virtum ps -a       # toutes les VM connues (y compris arrêtées)
virtum shell <vm-id>
virtum stop <vm-id> [vm…]      # une ou plusieurs VM (id ou name)
virtum start <vm-id> [vm…]     # relancer une ou plusieurs VM arrêtées
virtum destroy <vm-id> [vm…]   # id/name ; orphelin libvirt (`virsh list` sans `virtum ps`)
```

Virtum utilise **libvirt système** (`qemu:///system`) et le réseau NAT **`default`** (IP invitée typique `192.168.122.x`, comme VMware NAT).

Le stockage actif est **`/var/lib/virtum`** (images, instances, clés, `metadata.json`) — pas `~/.virtum`, après `setup-var-lib.sh`. `virtum doctor` doit afficher `[ok] Stockage Virtum`.

## Validation MVP

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

## Licence

À définir.
