# Virtum

[English](./README.md) | **Français**

**La VM, tout simplement.**

Virtum lance des machines virtuelles locales et des labs multi-VM avec une expérience proche de Docker : `virtum run`, `virtum ps`, `virtum shell`, et un fichier `Virtumfile` pour décrire un lab entier. Les VM tournent sur KVM et libvirt, sur votre propre machine Linux.

```bash
virtum run --os debian-13 --name web   # une VM Debian, prête en une minute
virtum shell web                       # session SSH
virtum up                              # le lab décrit dans ./Virtumfile
```

![Démo : virtum up crée un lab de deux VM, nginx répond sur le port redirigé, virtum shell ouvre une session dans la VM](./assets/demo.gif)

## Nouveautés

| Version | Ce qu'elle apporte |
| :--- | :--- |
| [**1.4.1**](https://github.com/Marvinviverge/virtum/releases/tag/v1.4.1) · 26/11/2026 | **Fedora et Arch Linux** validés ; ports corrigés sur les systèmes sans `which` ; dossiers partagés sur Arch. |
| [**1.4.0**](https://github.com/Marvinviverge/virtum/releases/tag/v1.4.0) · 24/11/2026 | **Virtum en français ou en anglais**, aide comprise, selon la langue du système ; `virtum config set language en` pour choisir. |
| [**1.3.0**](https://github.com/Marvinviverge/virtum/releases/tag/v1.3.0) · 23/11/2026 | **Debian 13 et Ubuntu 26.04** par défaut ; installation corrigée sur Debian 12 ; images épinglées sur des versions datées. |
| [**1.2.0**](https://github.com/Marvinviverge/virtum/releases/tag/v1.2.0) · 22/11/2026 | **Clé SSH de chaque VM épinglée dès sa création** ; une VM ne peut plus envoyer de séquences au terminal. |
| [**1.1.0**](https://github.com/Marvinviverge/virtum/releases/tag/v1.1.0) · 20/11/2026 | **Paquet Debian** (`.deb`) avec chaque release, signé par attestation de provenance. |

Historique complet : [CHANGELOG.md](./CHANGELOG.md).

## Pourquoi Virtum

- **Simple** : une commande pour une VM, un fichier YAML pour un lab. Pas de Vagrantfile en Ruby, pas de console web.
- **Rapide** : images cloud officielles, disques en copie sur écriture, création des machines en parallèle.
- **Reproductible** : `plan` montre ce qui va changer, `apply` l'applique, les instantanés ramènent un lab à un état connu.
- **Sûr par défaut** : partages en lecture seule, clé SSH épinglée par VM, ports locaux sauf exposition confirmée. Voir [SECURITY.md](./SECURITY.md).
- **Open source** : licence Apache-2.0.

## Installation

Prérequis : Linux x86_64 avec KVM (virtualisation activée dans le BIOS/UEFI), glibc 2.34 ou plus récente. Testé de bout en bout sur Ubuntu 24.04, Debian 13, Debian 12, Fedora 45 et Arch Linux (Fedora et Arch : Virtum 1.4.1 ou plus récent) ; Ubuntu 22.04 n'est pas encore validé.

KVM est indispensable : un poste Linux ou un serveur dédié conviennent, mais un VPS classique n'expose généralement pas KVM. Vérifiez que `/dev/kvm` existe.

**Debian** : le réseau libvirt `default` n'y démarre pas tout seul. Après l'installation, une fois :

```bash
virsh -c qemu:///system net-autostart default && virsh -c qemu:///system net-start default
```

Sur Debian 12 et Ubuntu 22.04, les dossiers partagés (`mounts:`) ne sont pas disponibles : le paquet `virtiofsd` n'y existe pas. VM, labs, ports, Ansible et instantanés y fonctionnent. De même sur Fedora avec SELinux en mode enforcing (voir plus bas).

**Dans une VM (virtualisation imbriquée)** : Virtum fonctionne aussi dans une VM Linux, si l'hyperviseur lui expose la virtualisation (`/dev/kvm` existe dans la VM : CPU en mode `host-passthrough`, `nested=1` sur le module `kvm_intel` ou `kvm_amd` de l'hôte). Validé avec Virtum 1.4.0 dans une VM Debian 13 sur un hôte Ubuntu 24.04 sous KVM/libvirt (Proxmox, VMware et Hyper-V non testés) : VM, labs avec ports et provision, instantanés, dossiers partagés et `make smoke-lab`. La création d'une VM y prend environ 40 % de temps en plus que sur l'hôte nu (mesuré sur un portable : 16 s → 22 s pour une VM seule, 24 s → 34 s pour une VM de lab sans provision). Le réseau bridge n'a pas été testé dans ce montage. Ce montage ne rend pas Virtum multi-utilisateur : chaque VM a son propre Virtum, pour une seule personne. Pour un serveur commun avec une VM par personne, voir [SECURITY.md](./SECURITY.md).

Si la VM est elle-même sur un réseau `192.168.122.0/24` (le réseau libvirt `default` de l'hôte), le réseau `default` de la VM ne peut pas démarrer (`Network is already in use`). Déplacez-le sur un autre sous-réseau, une fois :

```bash
virsh -c qemu:///system net-dumpxml default | sed 's/192\.168\.122\./192.168.123./g' > default.xml
virsh -c qemu:///system net-define default.xml && virsh -c qemu:///system net-start default
```

### Depuis une release (recommandé)

Sur la [page des releases](https://github.com/Marvinviverge/virtum/releases), téléchargez `SHA256SUMS` et le paquet `.deb` (Debian, Ubuntu) ou l'archive `.tar.gz` (autres distributions).

**Paquet Debian/Ubuntu**

```bash
sha256sum -c --ignore-missing SHA256SUMS    # le fichier n'a pas été altéré
sudo apt install ./virtum_*_amd64.deb       # installe aussi libvirt, QEMU, socat…
sudo usermod -aG libvirt,kvm $USER          # puis reconnectez votre session
sudo /usr/share/virtum/setup-var-lib.sh     # prépare /var/lib/virtum
virtum doctor
```

**Archive**

```bash
sha256sum -c --ignore-missing SHA256SUMS
tar xzf virtum-*-x86_64-linux.tar.gz && cd virtum-*-x86_64-linux
sudo ./scripts/setup-host.sh                # une fois par machine : paquets, libvirt, groupes
install -D -m 755 virtum ~/.local/bin/virtum
sudo ./scripts/setup-var-lib.sh             # prépare /var/lib/virtum
virtum doctor
```

`~/.local/bin` doit être dans votre `PATH`. Après l'ajout aux groupes `libvirt` et `kvm`, reconnectez votre session. Tous les contrôles de `virtum doctor` doivent être `[ok]`.

**Fedora et Arch Linux** (Virtum 1.4.1 ou plus récent) : `setup-host.sh` ne gère que Debian et Ubuntu. À la place, installez les paquets ci-dessous, ajoutez votre compte aux groupes (`sudo usermod -aG libvirt,kvm $USER`, puis reconnectez-vous), et continuez avec `install` et `setup-var-lib.sh` ci-dessus.

```bash
# Fedora
sudo dnf install qemu-kvm libvirt-daemon-kvm libvirt-daemon-config-network libvirt-client \
  qemu-img cloud-utils openssh-clients socat
sudo systemctl start virtqemud.socket virtnetworkd.socket virtstoraged.socket \
  virtnodedevd.socket virtnwfilterd.socket virtlogd.socket virtlockd.socket
```

Sur Fedora, le réseau `default` démarre seul et choisit un sous-réseau libre. Avec SELinux en mode enforcing (le réglage par défaut), les dossiers partagés (`mounts:`) ne sont pas disponibles : Virtum les refuse avec un message clair. Tout le reste fonctionne.

```bash
# Arch Linux (redémarrez d'abord si le noyau vient d'être mis à jour)
sudo pacman -S qemu-base libvirt dnsmasq cloud-image-utils openssh socat virtiofsd bubblewrap
sudo systemctl enable --now libvirtd.service
sudo virsh -c qemu:///system net-autostart default && sudo virsh -c qemu:///system net-start default
```

Pour vérifier que le fichier a été construit par la CI du dépôt (GitHub CLI 2.49 ou plus récent) : `gh attestation verify <fichier> -R Marvinviverge/virtum`.

### Depuis les sources

Avec [Rust](https://rustup.rs) installé :

```bash
sudo ./scripts/setup-host.sh
make install                  # compile et installe dans ~/.cargo/bin
sudo ./scripts/setup-var-lib.sh
virtum doctor
```

Paquets installés par `setup-host.sh` (Debian/Ubuntu) : `qemu-kvm`, `libvirt-daemon-system`, `libvirt-clients`, `qemu-utils`, `cloud-image-utils`, `openssh-client`, `socat`, `virtiofsd`, `bubblewrap`. Sur une autre distribution, installez leurs équivalents et ajoutez votre compte aux groupes `libvirt` et `kvm`.

## Démarrage rapide

### Une VM

```bash
virtum run --os debian-13 --name web              # Debian 13 (aussi : ubuntu-26.04)
virtum run --os ubuntu-26.04 --ram 4096 --cpu 2 --disk-size 20G
virtum ps                                         # VM en cours, IP ; -a pour toutes
virtum shell web
virtum stop web && virtum start web
virtum destroy web
```

Images proposées : `debian-13` et `ubuntu-26.04`. `debian-12` et `ubuntu-24.04` restent acceptées pour les labs existants.

La première utilisation d'un OS télécharge son image cloud (`virtum pull <os>` pour le faire à l'avance).

### Un lab

Un `Virtumfile` décrit plusieurs machines sur un réseau isolé :

```yaml
name: demo-lab

machines:
  web:
    os: debian-13
    ram: 2048
    cpu: 2
    ports:
      - "18080:80"          # http://127.0.0.1:18080 → port 80 de la VM
    provision:
      - apt-get update
      - apt-get install -y nginx

  db:
    os: ubuntu-26.04
    ram: 1024
```

```bash
virtum plan                    # ce qui va être créé ou modifié, sans rien toucher
virtum up                      # crée ou relance le lab
virtum shell web
virtum down                    # arrête le lab ; --destroy pour le supprimer
```

Exemple complet : [`examples/Virtumfile`](./examples/Virtumfile).

## Fonctions

### Commandes

| Commande | Rôle |
| :--- | :--- |
| `doctor` | Vérifie que l'hôte est prêt (KVM, libvirt, groupes, stockage, outils) |
| `run`, `ps`, `shell`, `stop`, `start`, `destroy` | Cycle de vie d'une VM |
| `pull` | Télécharge une image d'OS en cache |
| `plan`, `up`, `apply`, `down` | Cycle de vie d'un lab décrit par un Virtumfile |
| `snapshot create\|restore\|ls\|rm` | Instantanés d'une VM ou d'un lab entier |
| `inventory`, `provision` | Inventaire et playbook Ansible du lab |
| `open`, `close` | Pare-feu de l'hôte (ufw) pour les ports exposés au réseau local |
| `config` | Réglages de l'utilisateur : langue de la sortie |

`virtum <commande> --help` détaille chaque option.

### Réseau

- Chaque lab a son **réseau libvirt isolé** (`virtum-lab-<nom>`) ; les machines d'un lab se joignent entre elles.
- `subnet: 10.42.0.0/24` fixe le sous-réseau du lab ; sinon il est calculé depuis le nom. `plan` et `up` refusent un sous-réseau qui chevauche un réseau existant.
- `ip: 10.42.0.20` donne une adresse fixe à une machine.
- `ports:` redirige un port de l'hôte vers la VM, sur `127.0.0.1`. Avec `bind: lan`, il écoute sur le réseau local, après confirmation ; `virtum open` ouvre alors le pare-feu.
- **Bridge** : `virtum run --network bridge` ou `network: bridge` dans un lab place la VM directement sur le réseau local, avec sa propre adresse. Ethernet recommandé, le Wi-Fi est souvent incompatible. Un lab peut mêler machines en bridge et machines privées.

### Dossiers partagés

```yaml
machines:
  web:
    os: debian-13
    mounts:
      - ./src:~/src             # lecture seule (par défaut)
      - ./out:~/out:rw          # la VM peut écrire
```

Les partages passent par virtiofs. La lecture seule est imposée côté hôte. Un partage `rw` ne peut contenir ni le Virtumfile ni les fichiers Ansible, et les partages ne s'imbriquent pas. `down --destroy --purge` supprime aussi les dossiers créés pour les partages.

### Ansible

```yaml
ansible:
  playbook: ansible/site.yml
  requirements: ansible/requirements.yml   # optionnel

machines:
  web:
    os: debian-13
    groups: [front]
```

`virtum up` joue le playbook au premier déploiement, puis prend l'instantané de lab `provisioned` (`virtum snapshot restore provisioned` y ramène). `virtum provision` le rejoue, `virtum inventory` produit l'inventaire (INI ou YAML) avec les clés SSH de chaque VM. Le playbook tourne sur une copie privée de son dossier, sans les partages inscriptibles.

### Instantanés

```bash
virtum snapshot create avant-test      # tout le lab (ou : avant-test web db)
virtum snapshot restore avant-test
virtum snapshot ls
virtum snapshot rm avant-test
```

Instantanés à froid des disques : les VM sont arrêtées le temps de l'opération puis relancées.

### Autres options du Virtumfile

| Champ | Rôle |
| :--- | :--- |
| `ram`, `cpu`, `disk_size` | Ressources ; `apply` les met à jour sur des VM existantes |
| `provision:` | Commandes shell jouées au premier démarrage |
| `ssh_port:` | Port SSH si la VM le déplace (repli sur 22) |
| `restart: no \| always \| unless-stopped` | Démarrage au boot de l'hôte, comme dans Docker Compose |

Retirer une machine du Virtumfile ne la supprime pas : `plan` la signale, `apply --prune` la supprime.

## Langue

Virtum affiche ses messages et son aide en français ou en anglais, selon la langue du système : français si la locale est française (`LANG=fr_FR.UTF-8`…), anglais sinon. Pour choisir :

```bash
virtum config set language en     # ou fr ; enregistré dans ~/.config/virtum/config.yml
virtum config get language        # langue active et son origine
virtum config unset language      # revient à la langue du système
VIRTUM_LANG=en virtum up          # pour une seule commande
```

## Stockage

Virtum utilise libvirt système (`qemu:///system`) et range images, instances, clés et état dans `/var/lib/virtum` (préparé par `setup-var-lib.sh`), sinon dans `~/.virtum`.

## Sécurité

Modèle de menace, garanties et limites : [SECURITY.md](./SECURITY.md). Pour signaler une faille : **security@virtum.dev**.

## Contribuer

Les contributions sont bienvenues : voir [CONTRIBUTING.md](./CONTRIBUTING.md). `make check` lance le format, clippy, les tests et l'audit des dépendances ; `make smoke-lab` vérifie un vrai lab de bout en bout.

## Licence

Virtum est distribué sous licence [Apache 2.0](./LICENSE) : usage, modification et redistribution libres, y compris commerciaux, en conservant la licence et les mentions de copyright. Historique des versions : [CHANGELOG.md](./CHANGELOG.md).
