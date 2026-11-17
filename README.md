# Virtum

**Français** | [English](./README.en.md)

**La VM, tout simplement.**

Virtum lance des machines virtuelles locales et des labs multi-VM avec une expérience proche de Docker : `virtum run`, `virtum ps`, `virtum shell`, et un fichier `Virtumfile` pour décrire un lab entier. Les VM tournent sur KVM et libvirt, sur votre propre machine Linux.

```bash
virtum run --os debian-12 --name web   # une VM Debian, prête en une minute
virtum shell web                       # session SSH
virtum up                              # le lab décrit dans ./Virtumfile
```

## Pourquoi Virtum

- **Simple** : une commande pour une VM, un fichier YAML pour un lab. Pas de Vagrantfile en Ruby, pas de console web.
- **Rapide** : images cloud officielles, disques en copie sur écriture, création des machines en parallèle.
- **Reproductible** : `plan` montre ce qui va changer, `apply` l'applique, les instantanés ramènent un lab à un état connu.
- **Sûr par défaut** : partages en lecture seule, clé SSH épinglée par VM, ports locaux sauf exposition confirmée. Voir [SECURITY.md](./SECURITY.md).
- **Open source** : licence Apache-2.0.

## Installation

Prérequis : Linux x86_64 avec KVM (virtualisation activée dans le BIOS/UEFI). Testé sur Ubuntu 24.04 ; le binaire demande glibc 2.34 ou plus récente (Debian 12, Ubuntu 22.04 et suivantes), mais ces systèmes n'ont pas encore été validés de bout en bout.

### Depuis une release (recommandé)

Téléchargez `virtum-<version>-x86_64-linux.tar.gz` et `SHA256SUMS` depuis la [page des releases](https://github.com/Marvinviverge/virtum/releases), puis :

```bash
sha256sum -c SHA256SUMS                     # l'archive n'a pas été altérée
gh attestation verify virtum-*-x86_64-linux.tar.gz -R Marvinviverge/virtum   # optionnel : construite par la CI du dépôt
tar xzf virtum-*-x86_64-linux.tar.gz && cd virtum-*-x86_64-linux
sudo ./scripts/setup-host.sh                # une fois par machine : paquets, libvirt, groupes
install -D -m 755 virtum ~/.local/bin/virtum
sudo ./scripts/setup-var-lib.sh             # prépare /var/lib/virtum
virtum doctor
```

`~/.local/bin` doit être dans votre `PATH`. Après `setup-host.sh`, reconnectez votre session pour entrer dans les groupes `libvirt` et `kvm`. Tous les contrôles de `virtum doctor` doivent être `[ok]`. `gh attestation verify` demande GitHub CLI 2.49 ou plus récent.

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
virtum run --os debian-12 --name web              # Debian 12 (aussi : ubuntu-24.04, ubuntu-26.04)
virtum run --os ubuntu-24.04 --ram 4096 --cpu 2 --disk-size 20G
virtum ps                                         # VM en cours, IP ; -a pour toutes
virtum shell web
virtum stop web && virtum start web
virtum destroy web
```

La première utilisation d'un OS télécharge son image cloud (`virtum pull <os>` pour le faire à l'avance).

### Un lab

Un `Virtumfile` décrit plusieurs machines sur un réseau isolé :

```yaml
name: demo-lab

machines:
  web:
    os: debian-12
    ram: 2048
    cpu: 2
    ports:
      - "18080:80"          # http://127.0.0.1:18080 → port 80 de la VM
    provision:
      - apt-get update
      - apt-get install -y nginx

  db:
    os: ubuntu-24.04
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
    os: debian-12
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
    os: debian-12
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

## Stockage

Virtum utilise libvirt système (`qemu:///system`) et range images, instances, clés et état dans `/var/lib/virtum` (préparé par `setup-var-lib.sh`), sinon dans `~/.virtum`.

## Sécurité

Modèle de menace, garanties et limites : [SECURITY.md](./SECURITY.md). Pour signaler une faille : **security@virtum.dev**.

## Contribuer

Les contributions sont bienvenues : voir [CONTRIBUTING.md](./CONTRIBUTING.md). `make check` lance le format, clippy, les tests et l'audit des dépendances ; `make smoke-lab` vérifie un vrai lab de bout en bout.

## Licence

Virtum est distribué sous licence [Apache 2.0](./LICENSE) : usage, modification et redistribution libres, y compris commerciaux, en conservant la licence et les mentions de copyright. Historique des versions : [CHANGELOG.md](./CHANGELOG.md).
