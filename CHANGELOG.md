# Changelog

Les versions suivent le [versionnage sémantique](https://semver.org/lang/fr/).

## [1.2.1] - 2026-11-23

### Corrections

- **Images Debian 12 et Ubuntu 24.04 introuvables** : Virtum les téléchargeait depuis une adresse « dernière version », qui change à chaque nouvelle image publiée, alors que leur empreinte attendue est fixe. Depuis les images publiées en novembre, le téléchargement était refusé (« Checksum invalide ») pour tout nouvel utilisateur. Les images sont maintenant épinglées sur une version datée (Debian 12 du 17/11/2026, Ubuntu 24.04 du 17/10/2026), dans un fichier qui porte la date.
- **Les VM existantes gardent leur image** : une VM qui redémarre, ou dont le disque est agrandi, utilise l'image de base de son disque, et non l'image actuelle de son OS. Rien n'est re-téléchargé ni remplacé.
- **Libvirt système toujours utilisé** : Virtum appelle `virsh -c qemu:///system`. Jusqu'ici, il comptait sur `LIBVIRT_DEFAULT_URI`, que seul Ubuntu pose, et seulement dans un shell de connexion : sur Debian, ou depuis cron ou `ssh hôte virtum …`, virsh visait le libvirt de session.
- **Ports sur Debian 12 et Ubuntu 22.04** : avec socat antérieur à 1.8 (sans `ACCEPT-FD`), les `ports:` échouaient. socat ouvre alors le port lui-même (voir SECURITY.md, Limites).
- **Dossiers partagés sans virtiofsd compatible** : refus immédiat avec un message clair, au lieu d'une attente puis d'une erreur de socket. `virtum doctor` signale virtiofsd comme optionnel : son absence ne bloque plus le diagnostic.
- Commandes proposées par `virtum doctor` pour le réseau `default` : `virsh -c qemu:///system …`.

### Plateformes

Testé de bout en bout sur Debian 12 (paquet `.deb`, `virtum doctor`, VM, lab avec ports et provision). Sur Debian 12, démarrer une fois le réseau `default` (voir README).

## [1.2.0] - 2026-11-22

### Sécurité

- **Clé d'hôte SSH épinglée dès la création** : Virtum génère la clé d'hôte de chaque nouvelle VM, la lui remet par cloud-init et l'inscrit dans son `known_hosts` avant le premier démarrage. La première connexion n'accepte plus une clé inconnue : une machine qui répondrait à la place de la VM (autre VM du lab, machine du LAN en bridge) est refusée. Les VM créées par une version antérieure gardent leur clé enregistrée au premier contact.
- **Sortie des VM filtrée** : les messages d'erreur venus d'une VM ou d'un outil externe sont affichés sans caractères de contrôle. Une VM ne peut plus changer le titre du terminal, effacer l'écran ou écrire dans le presse-papiers via une séquence d'échappement.
- Dépendance `url` 2.5.9 (RUSTSEC-2026-0592).

### Changements

- Le site du projet est https://virtum.dev : lien dans `Cargo.toml` et champ `Homepage` du paquet Debian.
- Modèles d'issues (bug, idée) en français et en anglais ; les failles sont renvoyées vers le signalement privé.

### Mise à jour

Rien à faire pour les labs existants : ils continuent de fonctionner. Pour qu'une VM existante profite de la clé d'hôte épinglée dès la création, recréez-la (`virtum destroy` puis `virtum run`, ou `virtum down --destroy` puis `virtum up`).

## [1.1.0] - 2026-11-20

### Ajouts

- **Paquet Debian** `virtum_<version>_amd64.deb` dans chaque release, avec sa propre empreinte dans `SHA256SUMS` et son attestation de provenance. `apt` installe libvirt, QEMU, qemu-utils, cloud-image-utils, openssh-client et socat ; `virtiofsd` et `bubblewrap` (dossiers partagés) sont recommandés. Les scripts de préparation de l'hôte sont installés dans `/usr/share/virtum/`.
- README et guide de contribution en anglais.

### Changements

- Binaire de release sans symboles de débogage : environ 8 Mo.

### Plateformes

Paquet installé et vérifié sur Ubuntu 24.04. Sur Ubuntu 22.04 et Debian 12, ses dépendances se résolvent, mais `virtiofsd` n'y existe pas en paquet séparé : les dossiers partagés n'y sont pas pris en charge.

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
