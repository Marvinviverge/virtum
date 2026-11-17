# Changelog

Les versions suivent le [versionnage sémantique](https://semver.org/lang/fr/).

## [1.0.0] - 2026-11-17

Première version publiée. Virtum lance des VM KVM locales et des labs multi-VM décrits dans un `Virtumfile`, sur libvirt (`qemu:///system`).

### Fonctions

- VM seules : `run`, `ps`, `shell`, `stop`, `start`, `destroy`, `pull`, `doctor`. Images Debian 12, Ubuntu 24.04 et Ubuntu 26.04, téléchargées et vérifiées par empreinte.
- Labs : `up`, `down`, `plan`, `apply [--prune]`, création en parallèle, réseau isolé par lab, `subnet:` et `ip:` déclarables.
- Réseau : ports locaux (`ports:`), exposition au LAN confirmée (`bind: lan`, `open` / `close`), carte bridge (macvtap) et labs hybrides.
- Dossiers partagés hôte ↔ VM (virtiofs), **en lecture seule par défaut** (`rw` explicite).
- Ansible intégré : `groups:`, `virtum inventory`, section `ansible:` jouée par `up`, `virtum provision`.
- Instantanés de VM ou de lab (`snapshot`), `ssh_port`, redémarrage au boot de l'hôte (`restart:`).

### Sécurité

Modèle de menace, garanties G1 à G7 et limites : [`SECURITY.md`](./SECURITY.md). Quatre audits de sécurité ont été menés avant cette version ; tous les constats confirmés sont corrigés.

- Partages en lecture seule imposée côté hôte ; un partage `rw` ne peut contenir ni le Virtumfile, ni le playbook, ni le fichier `requirements`, et les partages ne s'imbriquent pas.
- Ansible tourne sur une copie privée du dossier du playbook, sans les partages inscriptibles.
- Clé d'hôte SSH épinglée par VM (`HostKeyAlias`).
- Ressources libvirt marquées : Virtum ne touche pas aux domaines et réseaux d'un autre Virtum.
- Fichiers d'état écrits sans suivre de lien symbolique.
- Dépendances contrôlées en CI (`cargo audit`, `cargo deny`), aucune crate non maintenue ; binaire de release accompagné de `SHA256SUMS` et d'une attestation de provenance GitHub.

### Plateformes

Linux x86_64 avec KVM, libvirt, virtiofsd et bubblewrap. Testé sur Ubuntu 24.04 ; le binaire demande glibc 2.34 ou plus récente (Debian 12, Ubuntu 22.04 et suivantes, non encore validés de bout en bout).

### Licence

Apache-2.0.
