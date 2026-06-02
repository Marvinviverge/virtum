# Virtum

**Virtum : la VM, tout simplement.**

CLI Linux pour lancer des VM locales reproductibles avec une expérience proche de Docker : `run`, `shell`, `stop`, `destroy`.

## Statut

**MVP v0.1 en cours de développement** — le dépôt contient la spécification ; l’implémentation Rust suit [`STEPS.md`](./STEPS.md).

Ne pas implémenter hors périmètre v0.1 (Virtumfile, `virtum up`, multi-VM, ports, mounts, etc.) : voir [`STEPS.md`](./STEPS.md) §1.

## Documentation

| Document | Rôle |
| :--- | :--- |
| [`PITCH.md`](./PITCH.md) | Vision produit et positionnement |
| [`PRD.md`](./PRD.md) | Exigences produit et roadmap long terme |
| [`SPEC_TECH_V0.1.md`](./SPEC_TECH_V0.1.md) | **Contrat technique du MVP** |
| [`STEPS.md`](./STEPS.md) | **Plan d’implémentation** (étapes 1–21) |

Le périmètre exécutable du MVP est défini par **SPEC + STEPS**, pas par l’intégralité du PRD.

## Prérequis hôte (Linux)

```bash
sudo apt update && sudo apt install -y \
  qemu-kvm libvirt-daemon-system libvirt-clients \
  qemu-utils cloud-image-utils openssh-client
```

Ajoutez votre utilisateur aux groupes `libvirt` et `kvm`, puis reconnectez la session.

## Usage cible (v0.1)

```bash
virtum doctor
virtum run --os debian-12
virtum shell <vm-id>
virtum stop <vm-id>
virtum destroy <vm-id>
```

## Licence

À définir.
