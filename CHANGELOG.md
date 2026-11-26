# Changelog

Les versions suivent le [versionnage sémantique](https://semver.org/lang/fr/).

## [1.4.1] - 2026-11-26

### Corrections

- **Fedora, Arch et systèmes sans la commande `which`** : les `ports:` échouaient (« socat est requis ») alors que socat était installé, et `virtum plan` pouvait signaler à tort socat, cloud-localds ou ufw manquants. Virtum cherche désormais les commandes dans le `PATH` lui-même.
- **virtiofsd sur Arch** (`/usr/lib/virtiofsd`) : il est trouvé, et les dossiers partagés fonctionnent.
- **SELinux en mode enforcing** (Fedora, RHEL) : il empêche QEMU de se connecter au virtiofsd lancé par Virtum. Les dossiers partagés sont désormais refusés tout de suite, avec un message clair, au lieu d'un échec « Permission denied » au démarrage de la VM ; `virtum doctor` le signale.
- Messages d'aide : le chemin des scripts de préparation de l'hôte (`setup-var-lib.sh`, `setup-apparmor-virtiofs.sh`) est celui du paquet `.deb` (`/usr/share/virtum/`) ou du dossier de l'archive, et non plus un chemin de la machine de compilation.

### Documentation

- Le README par défaut du dépôt est désormais en anglais (`README.md`) ; la version française passe dans `README.fr.md`, à un clic depuis l'en-tête. Le paquet `.deb` et l'archive `.tar.gz` contiennent les deux.

### Plateformes

- **VM imbriquée** : validé avec Virtum 1.4.0 dans une VM Debian 13 (6 vCPU, 8 Go, CPU `host-passthrough`) sur un hôte Ubuntu 24.04 (Ryzen 5 3550H, `kvm_amd nested=1`) : paquet `.deb`, `virtum doctor` (15/15), VM seule, lab d'exemple avec ports et provision, instantané de lab, dossiers partagés, `make smoke-lab`. Création d'une VM environ 40 % plus lente que sur l'hôte nu. Si la VM est sur le réseau `default` de l'hôte (`192.168.122.0/24`), déplacer le réseau `default` de la VM sur un autre sous-réseau (voir README). Réseau bridge non testé dans ce montage. Hyperviseur testé : KVM/libvirt (Proxmox, VMware, Hyper-V non testés). SECURITY.md : recommandations pour un serveur commun avec une VM par personne.
- **Fedora 45** (archive `.tar.gz`, SELinux en mode enforcing) et **Arch Linux** (archive, système à jour au 25/11/2026) : validés avec Virtum 1.4.1, chacun dans une VM (6 vCPU, 8 Go, virtualisation imbriquée) sur un hôte Ubuntu 24.04. `virtum doctor` (15/15), VM seule, lab d'exemple avec ports et provision, instantané de lab, `make smoke-lab`, sans la commande `which`. Dossiers partagés : fonctionnent sur Arch ; indisponibles sur Fedora avec SELinux en mode enforcing. Paquets à installer : voir README (`setup-host.sh` ne gère que Debian et Ubuntu). Avec Virtum 1.4.0, les `ports:` échouent sur ces deux systèmes sans `which`, et les dossiers partagés sur Arch.

## [1.4.0] - 2026-11-24

### Ajouts

- **Virtum en français ou en anglais.** Tous les messages (progression, erreurs et leurs pistes de résolution, `virtum doctor`, `virtum plan`, questions de confirmation) et l'aide (`--help`) existent dans les deux langues.
  - Sans réglage, Virtum suit la langue du système : français si la locale est française (`LC_ALL`, `LC_MESSAGES`, `LANG`), anglais sinon.
  - `virtum config set language en|fr` enregistre un choix dans `~/.config/virtum/config.yml` ; `virtum config get language` affiche la langue active et son origine ; `virtum config unset language` revient à la langue du système.
  - `VIRTUM_LANG=en` ou `fr` force la langue d'une seule commande.
- Questions de confirmation : `[o/N]` en français (`o` ou `y` acceptés), `[y/N]` en anglais.

### Changements

- **Sur un système qui n'est pas en français, Virtum s'affiche désormais en anglais.** Sur un système en français, la sortie ne change pas, à deux détails près : le préfixe d'erreur devient « erreur : » et les confirmations « [o/N] ».
- Les formats lus par des programmes ne changent pas : en-têtes de `virtum ps` et `virtum snapshot ls`, inventaire Ansible, codes de sortie.
- Limite : les erreurs de syntaxe de la ligne de commande produites par clap (« unexpected argument ») restent en anglais.

## [1.3.0] - 2026-11-23

### Ajouts

- **Image Debian 13** (`debian-13`), la version stable de Debian. Elle devient l'OS par défaut de `virtum run`, et le README, l'exemple et la documentation utilisent désormais `debian-13` et `ubuntu-26.04`. `debian-12` et `ubuntu-24.04` restent acceptées : aucun Virtumfile existant n'est cassé.
- **Debian 13 comme hôte**, testé de bout en bout, dossiers partagés compris (`virtiofsd` et socat 1.8 y sont empaquetés).

### Corrections

- **VM Debian 13 qui redémarraient en boucle** : leur GRUB plante quand la VM n'a aucune carte graphique, et libvirt n'en ajoute pas d'office. Les VM reçoivent une carte VGA sans affichage.
- **Images Debian 12 et Ubuntu 24.04 introuvables** : Virtum les téléchargeait depuis une adresse « dernière version », qui change à chaque nouvelle image publiée, alors que leur empreinte attendue est fixe. Depuis les images publiées en novembre, le téléchargement était refusé (« Checksum invalide ») pour tout nouvel utilisateur. Les images sont maintenant épinglées sur une version datée (Debian 12 du 17/11/2026, Ubuntu 24.04 du 17/10/2026), dans un fichier qui porte la date.
- **Les VM existantes gardent leur image** : une VM qui redémarre, ou dont le disque est agrandi, utilise l'image de base de son disque, et non l'image actuelle de son OS. Rien n'est re-téléchargé ni remplacé.
- **Libvirt système toujours utilisé** : Virtum appelle `virsh -c qemu:///system`. Jusqu'ici, il comptait sur `LIBVIRT_DEFAULT_URI`, que seul Ubuntu pose, et seulement dans un shell de connexion : sur Debian, ou depuis cron ou `ssh hôte virtum …`, virsh visait le libvirt de session.
- **Ports sur Debian 12 et Ubuntu 22.04** : avec socat antérieur à 1.8 (sans `ACCEPT-FD`), les `ports:` échouaient. socat ouvre alors le port lui-même (voir SECURITY.md, Limites).
- **Dossiers partagés sans virtiofsd compatible** : refus immédiat avec un message clair, au lieu d'une attente puis d'une erreur de socket. `virtum doctor` signale virtiofsd comme optionnel : son absence ne bloque plus le diagnostic.
- Commandes proposées par `virtum doctor` pour le réseau `default` : `virsh -c qemu:///system …`.

### Plateformes

Testé de bout en bout comme hôte sur Ubuntu 24.04, Debian 13 et Debian 12 (paquet `.deb`, `virtum doctor`, VM, lab avec ports et provision ; dossiers partagés sur Ubuntu 24.04 et Debian 13). Sur Debian, démarrer une fois le réseau `default` (voir README). KVM est indispensable : un VPS classique n'en donne généralement pas.

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
