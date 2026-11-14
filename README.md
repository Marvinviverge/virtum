# Virtum

**Virtum : la VM, tout simplement.**

CLI Linux pour lancer des VM locales reproductibles avec une expérience proche de Docker : `run`, `ps`, `shell`, `stop`, `start`, `destroy`, labs `up` / `down`, instantanés `snapshot`.

## Statut

**MVP v0.1 terminé** — cycle `doctor` → `run` → `ps` → `shell` → `stop` → `start` → `destroy` (voir [`STEPS.md`](./STEPS.md)).

**v0.2 terminé (lab-as-code)** — Virtumfile, `virtum up` / `down`, `pull`, `disk-size`, ports (`socat`), provision, réseau isolé **`virtum-lab-<name>`** : [`STEPS2.md`](./STEPS2.md), [`SPEC_TECH_V0.2.md`](./SPEC_TECH_V0.2.md). Exemple : [`examples/Virtumfile`](./examples/Virtumfile).

**v0.3 terminé** — `bind: lan`, `virtum open` / `close`, `virtum plan` / `apply`, `virtum run --network bridge`, lab **`network: bridge`** et **hybride** (dual NIC), `virtum ps` (IP + LAN) : [`STEPS3.md`](./STEPS3.md) §8, [`SPEC_TECH_V0.3.md`](./SPEC_TECH_V0.3.md). Validation : `make smoke-lab`.

**v0.4 terminé** — `mounts:` dossiers partagés hôte ↔ VM (**virtiofs**) : [`STEPS4.md`](./STEPS4.md) §7, [`SPEC_TECH_V0.4.md`](./SPEC_TECH_V0.4.md). Exemple : [`examples/Virtumfile.mounts`](./examples/Virtumfile.mounts).

**v0.5 terminée** — durcissement sécurité V-001–V-006 (guest mounts, cloud-init, SSH `known_hosts`, `--insecure-ssh`, confirmation `bind: lan`, `cargo audit`) : [`STEPS5.md`](./STEPS5.md), [`SPEC_TECH_V0.5.md`](./SPEC_TECH_V0.5.md).

**v0.6 terminée (banc de test d'infra)** — instantanés `virtum snapshot` (VM ou lab), `ssh_port` (VM durcies), `restart:` au boot de l'hôte, alias `virtum list`, fiabilité (statut libvirt réel, images de base, orphelins dans `doctor`) : [`SPEC_TECH_V0.6.md`](./SPEC_TECH_V0.6.md).

**v0.7 terminée (convergence)** — `ip:` déclarée (optionnelle) par machine, machines retirées du Virtumfile signalées par `plan` et supprimées par `apply --prune`, dérive RAM/vCPU lue dans libvirt : [`SPEC_TECH_V0.7.md`](./SPEC_TECH_V0.7.md).

**v0.8 terminée (Ansible intégré)** — `groups:` et `virtum inventory`, section `ansible:` jouée par `virtum up` puis instantané `provisioned`, `virtum provision` ; l'exemple [`examples/Virtumfile-kubernetes`](./examples/Virtumfile-kubernetes) monte un cluster Kubernetes `Ready` en un `virtum up` : [`SPEC_TECH_V0.8.md`](./SPEC_TECH_V0.8.md).

**v0.9 terminée (création parallèle, `subnet:`, temps de création)** — `virtum up` crée et reprend les machines d'un lab en parallèle (`--parallel N`, défaut 4 ; `--parallel 1` = une après l'autre), sortie préfixée par machine et récapitulatif ; verrou sur `metadata.json` ; `subnet:` déclarable et chevauchements de sous-réseaux détectés ; création plus rapide sans modifier l'image (3 VM Ubuntu : 216 s en v0.8 → 25 s) : [`SPEC_TECH_V0.9.md`](./SPEC_TECH_V0.9.md).

## Sécurité

Modèle de menace, garanties et limites : [`SECURITY.md`](./SECURITY.md).

## Documentation

| Document | Rôle |
| :--- | :--- |
| [`PITCH.md`](./PITCH.md) | Vision produit et positionnement |
| [`PRD.md`](./PRD.md) | Exigences produit et roadmap long terme |
| [`SPEC_TECH_V0.1.md`](./SPEC_TECH_V0.1.md) | **Contrat technique du MVP** |
| [`STEPS.md`](./STEPS.md) | Plan d’implémentation **v0.1** (terminé) |
| [`STEPS2.md`](./STEPS2.md) | Plan d’implémentation **v0.2** |
| [`SPEC_TECH_V0.2.md`](./SPEC_TECH_V0.2.md) | Contrat technique **v0.2** (lab, réseau, ports) |
| [`SPEC_TECH_V0.3.md`](./SPEC_TECH_V0.3.md) | Contrat technique **v0.3** (bridge, `bind: lan`, plan, pare-feu) |
| [`STEPS3.md`](./STEPS3.md) | Plan d’implémentation **v0.3** (terminé) |
| [`SPEC_TECH_V0.4.md`](./SPEC_TECH_V0.4.md) | Contrat technique **v0.4** (`mounts:`, virtiofs) |
| [`SPEC_TECH_V0.5.md`](./SPEC_TECH_V0.5.md) | Contrat technique **v0.5** (security hardening) |
| [`STEPS5.md`](./STEPS5.md) | Plan d’implémentation **v0.5** |
| [`specs/001-security-hardening/spec.md`](./specs/001-security-hardening/spec.md) | Spec Kit feature v0.5 |
| [`SPEC_TECH_V0.6.md`](./SPEC_TECH_V0.6.md) | Contrat technique **v0.6** (instantanés, `ssh_port`, `restart`) |
| [`SPEC_TECH_V0.7.md`](./SPEC_TECH_V0.7.md) | Contrat technique **v0.7** (convergence : `ip:`, `apply --prune`, dérive) |
| [`SPEC_TECH_V0.8.md`](./SPEC_TECH_V0.8.md) | Contrat technique **v0.8** (Ansible intégré : `groups:`, `virtum inventory`, `ansible:`, `virtum provision`) |
| [`SPEC_TECH_V0.9.md`](./SPEC_TECH_V0.9.md) | Contrat technique **v0.9** (création parallèle `virtum up --parallel`, `subnet:`, temps de création) |

Le périmètre exécutable du MVP est défini par **SPEC + STEPS**, pas par l’intégralité du PRD.

## Prérequis hôte (Linux)

- **Rust** (toolchain stable + `cargo`) — [rustup.rs](https://rustup.rs) ; requis pour compiler via `install.sh`.
- **cargo-audit** (optionnel en dev, recommandé CI) — `cargo install cargo-audit` ; intégré dans `make check` (avertissement si absent).
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
  qemu-utils cloud-image-utils openssh-client socat virtiofsd bubblewrap
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
| `virtum doctor` | Binaires, KVM, groupes, réseau `default`, **bridge LAN**, stockage, socat, images de base des VM, restes orphelins (avertissement) |

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
virtum run --os debian-12 --network bridge --name demo-lan   # IP du LAN (macvtap)
# Si échec IP : vérifiez `ip route` puis par ex. --bridge-interface wlp3s0 (interface du partage téléphone)
virtum plan -f examples/Virtumfile   # prévisualise create / update / resume (sans rien modifier)
virtum apply -f examples/Virtumfile  # applique RAM, bind, ports, disk_size sur VMs existantes
virtum apply --prune -f examples/Virtumfile  # v0.7 : supprime aussi les machines retirées du fichier (confirmation, ou --yes)
virtum up                    # crée ou relance le lab (Virtumfile) ; après `down`, relance sans recréer
virtum up --parallel 2       # v0.9 : au plus 2 machines créées / reprises à la fois (défaut 4, 1 = séquentiel)
virtum up -f examples/Virtumfile   # ex. bind: lan → http://<IP-PC>:18080 depuis le téléphone
virtum open -f examples/Virtumfile # sudo ufw allow (ports bind: lan)
virtum close -f examples/Virtumfile  # retire les règles ouvertes par virtum open
virtum down                  # arrête toutes les VM du lab (Virtumfile `name`)
virtum down --destroy        # supprime le lab
virtum up -f examples/Virtumfile.mounts   # lab v0.4 avec mounts virtiofs
virtum down -f examples/Virtumfile.mounts --destroy --purge  # + dossiers hôte mounts
virtum ps          # VM en cours (NAME, LAB, DISK, IP, …) ; signale les labs éteints
virtum ps -a       # toutes les VM connues (y compris arrêtées)
virtum list        # alias de ps
virtum shell <vm-id>
virtum stop <vm-id> [vm…]      # une ou plusieurs VM (id ou name)
virtum start <vm-id> [vm…]     # relancer une ou plusieurs VM arrêtées
virtum destroy <vm-id> [vm…]   # id/name ; orphelin libvirt (`virsh list` sans `virtum ps`)
virtum snapshot create|restore|ls|rm <nom> [vm…]   # instantanés (sans vm : tout le lab)
```

Virtum utilise **libvirt système** (`qemu:///system`).

- **`virtum run`** (VM seule) : réseau libvirt **`default`** (`192.168.122.x`). Option **`--network bridge`** : macvtap sur l’interface physique (IP LAN via DHCP). Voir `virtum doctor` ; Wi‑Fi / partage téléphone souvent instable.
- **`virtum up`** (lab) : réseau dédié **`virtum-lab-<name>`** par lab (NAT isolé). Les machines du même lab se joignent entre elles.
- **`bind: lan`** (Virtumfile) : les `ports:` écoutent sur `0.0.0.0` (joignable depuis le même Wi‑Fi/Ethernet que le PC). Ouvrez le pare-feu hôte avec **`virtum open`** après `virtum up`. **`virtum plan`** compare les ports du Virtumfile à l’état enregistré dans `firewall-<lab>.json` (aligné, absent, ou désaligné après changement de port).
- **Inventaire Ansible (v0.8)** : `virtum inventory -f Virtumfile [-o inventory.ini] [--format yaml]` liste les machines qui tournent (nom de machine, IP, port, clé et `known_hosts` de l'instance : pas besoin de `host_key_checking = False`). Groupes : `groups: [web, app]` par machine, plus un groupe implicite au nom du lab (`-` → `_`). Ex. `virtum inventory -o inventory.ini && ansible all -i inventory.ini -m ping`.
- **Provisionnement Ansible (v0.8)** : section `ansible:` du Virtumfile (`playbook:`, `requirements:` optionnel, chemins relatifs au Virtumfile) et `virtum provision -f Virtumfile [-- <arguments ansible-playbook>]`. Toutes les machines doivent tourner ; le playbook est lancé depuis une **copie privée de son dossier** (l'`ansible.cfg` voisin est lu, `-i` pointe sur l'inventaire Virtum), d'où sont exclus les partages `rw` : rien de ce qu'une VM peut écrire n'est exécuté sur l'hôte. Ce qui est chargé hors du dossier du playbook (`../commun`) n'est pas copié : placez rôles et fichiers à côté du playbook ; requirements réinstallés seulement s'ils changent ; code de sortie d'Ansible repris. État : `ansible-<lab>.json`, inventaire : `ansible-<lab>.inventory.ini` (répertoire d'état Virtum). **`virtum up`** joue le playbook quand le lab n'a jamais été provisionné avec succès ou que des machines viennent d'être créées, puis prend l'instantané de lab `provisioned` (`ansible.snapshot: false` pour s'en passer) : `virtum snapshot restore provisioned` ramène au lab sain. En cas d'échec du playbook, les VM sont conservées et le `up` suivant le rejoue. `virtum plan` et `virtum doctor` affichent l'état Ansible.
- **Machines retirées du Virtumfile (v0.7)** : elles restent déployées. `virtum plan` les liste (`- remove`, avec le nombre d'instantanés perdus) et `virtum up` le rappelle. `virtum apply --prune` les supprime comme `virtum destroy` (VM, disque, instantanés, réservation `ip:`) après confirmation ; `--yes` pour les scripts, refus sans terminal sinon. Les dossiers `mounts:` de l'hôte sont conservés.
- **Temps de création (v0.9)** : `qemu-guest-agent` n'est plus installé que pour une carte en `network: bridge` (sur un réseau de lab, l'IP vient des baux DHCP) ; le réseau de lab utilise `no-ping` et `dhcp-ignore-clid` (dnsmasq) : bail immédiat, et une machine recréée retrouve son adresse. Un réseau de lab créé avant la v0.9 reçoit ces réglages quand il est recréé (automatiquement s'il n'a plus de VM).
- **`subnet:` du lab (v0.9, optionnel)** : `subnet: 10.42.0.0/24` fixe le réseau du lab (/24 privé) ; sans ce champ, il est calculé depuis le nom du lab (`192.168.X.0/24`, affiché par `virtum plan`). `plan` et `up` refusent un sous-réseau qui chevauche un autre réseau libvirt ou une interface de l'hôte, et proposent un sous-réseau libre. Changer `subnet:` : `down --destroy` puis `up` si le lab a des VM.
- **`ip:` par machine (v0.7, optionnel)** : adresse fixe sur le réseau du lab (`ip: 192.168.68.20`), via une réservation DHCP libvirt ; l'invité reste en DHCP. Le sous-réseau du lab est affiché par `virtum plan`. Ajout ou changement : `virtum apply` (redémarre la VM). Sans `ip:`, DHCP libre comme avant. Impossible en `network: bridge` seul.
- **Virtumfile `network: bridge`** : lab entier sur le LAN (`examples/Virtumfile.bridge-lab`) ou mix web LAN + db privée (`examples/Virtumfile.bridge-mixed`, dual NIC). `virtum ps` affiche **IP** et **LAN**.
- **VM isolée** sur le LAN : `virtum run --network bridge`.

### Workflow lab recommandé (v0.3)

```bash
virtum plan -f examples/Virtumfile    # images manquantes ? virtum pull …
                                      # pare-feu : aligné / absent / désaligné ?
virtum apply -f examples/Virtumfile   # si ~ update (RAM, bind: lan, ports, disk_size)
virtum pull debian-12                 # si le plan le signale
virtum up -f examples/Virtumfile
virtum open -f examples/Virtumfile    # si bind: lan + ports (ufw, sudo)
```

Après `virtum down`, modifier le Virtumfile (ex. plus de RAM) puis **`virtum apply`** — pas besoin de `--destroy` pour ces champs.

Si vous changez un port hôte (`18080` → `18088`) : **`virtum apply`** met à jour socat, puis **`virtum close`** et **`virtum open`** pour resynchroniser ufw (le plan signale le décalage tant que le JSON n’est pas à jour).

### Port SSH (`ssh_port`)

Si la configuration de la VM (Ansible, provision) déplace sshd, déclarez le port dans le Virtumfile :

```yaml
name: infra-local
ssh_port: 2222        # tout le lab (défaut : 22)
machines:
  app:
    os: ubuntu-24.04
  edge:
    os: ubuntu-24.04
    ssh_port: 2200    # surcharge pour une machine
```

- `up`, `start`, `shell`, `apply` et les instantanés essaient ce port, puis **22 en repli** : une VM neuve (sshd encore sur 22) démarre sans changer le Virtumfile.
- Un changement de `ssh_port` est pris en compte au prochain `virtum up` (pas besoin d'`apply`) ; `virtum plan` l'affiche.
- `virtum ps` affiche `IP:port` quand le port n'est pas 22.

### Démarrage au boot de l'hôte (`restart`)

Comme `restart:` dans docker compose, au niveau du lab ou par machine :

```yaml
name: infra-local
restart: unless-stopped   # no | always | unless-stopped
machines:
  app:
    os: ubuntu-24.04
  scratch:
    os: ubuntu-24.04
    restart: no
```

| Valeur | Au boot de l'hôte |
| :--- | :--- |
| `no` | la VM reste éteinte |
| `always` | la VM démarre, même après `virtum stop` / `down` |
| `unless-stopped` | la VM démarre, sauf si vous l'avez arrêtée (`stop`, `down`) |
| *(absent)* | Virtum ne touche pas à l'autostart libvirt (comportement d'avant) |

- Repose sur l'autostart libvirt (`virsh autostart`) ; le réseau `virtum-lab-<name>` est déjà en autostart.
- Pris en compte au prochain `virtum up` ou `virtum apply` ; `virtum plan` l'affiche.
- **Limites** : au boot, libvirt démarre la VM seule. Les `ports:` (socat) ne reviennent qu'avec `virtum up`, et `restart` est refusé avec `mounts:` (virtiofsd ne serait pas lancé).
- Sans `restart`, `virtum ps` signale les VM de lab éteintes (par exemple après un redémarrage) et suggère `virtum up`.

### Instantanés (`virtum snapshot`)

Pour rejouer un scénario de test : figer l'état des disques avant un test risqué, puis y revenir.

```bash
virtum snapshot create avant-tls          # tout le lab du ./Virtumfile (ou -f …)
virtum snapshot create avant-tls app edge # seulement ces VM
virtum snapshot ls                        # instantanés + place disque
virtum snapshot restore avant-tls         # retour de tout le lab
virtum snapshot rm avant-tls
```

- **À froid** : seul le disque est capturé, pas la mémoire. Revenir à un instantané redémarre la VM depuis ce disque.
- **VM allumées** : `create` / `rm` les arrêtent proprement, `restore` les coupe (l'état courant est abandonné), puis toutes sont relancées. `--no-restart` les laisse éteintes.
- **Lab cohérent** : toutes les VM sont arrêtées avant le premier instantané. `restore` refuse sans rien toucher si une VM du lab n'a pas l'instantané.
- **Stockage** : instantanés internes au `disk.qcow2` de chaque VM (tag `virtum-<nom>`). Aucun coût à la création ; chaque bloc réécrit ensuite est conservé en double. `rm` libère l'espace pour réutilisation, sans forcément réduire le fichier.
- **Non inclus** : les dossiers hôte `mounts:`. `destroy` / `down --destroy` suppriment les instantanés avec le disque.
- Si `apply` a agrandi un disque après l'instantané, `restore` remet l'ancienne taille : `virtum plan` le signale.

### Réseau bridge (LAN) — prérequis hôte

| Sujet | Recommandation |
| :--- | :--- |
| **Câble** | Préférez **Ethernet** ; le Wi‑Fi est souvent incompatible ou instable avec macvtap / bridge vers le LAN. |
| **Diagnostic** | `virtum doctor` et `virtum plan` affichent l’interface et les avertissements. |
| **Accès LAN sans bridge lab** | `bind: lan` + `ports:` + `virtum open` (socat sur l’hôte, VM reste sur NAT lab). |
| **Pare-feu** | `virtum open` / `close` (ufw + `firewall-<lab>.json`) ; `virtum plan` vérifie l’alignement avec le Virtumfile. |
| **Entreprise** | DHCP filtré, VLAN invité ou isolation Wi‑Fi peuvent bloquer le LAN ; restez en `lab` NAT si besoin. |

Voir [`SPEC_TECH_V0.3.md`](./SPEC_TECH_V0.3.md) et [`STEPS3.md`](./STEPS3.md).

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

### Lab v0.2 / v0.3 (clôture v0.3)

Validation automatique du lab NAT :

```bash
make smoke-lab
```

```bash
virtum plan -f examples/Virtumfile
virtum apply -f examples/Virtumfile  # si modifications sur VMs existantes
virtum up -f examples/Virtumfile
virtum open -f examples/Virtumfile   # si bind: lan
```

Smoke tests (après `make release`) :

```bash
make smoke-lab                    # NAT : examples/Virtumfile (ping web↔db, curl :18098)
VIRTUM_BRIDGE_SMOKE=1 make smoke-lab-bridge   # hybride (Ethernet recommandé)
# ou : chmod +x scripts/smoke-test-lab.sh && ./scripts/smoke-test-lab.sh
```

Parcours hybride manuel (STEPS3 §7) : `virtum up -f examples/Virtumfile.bridge-mixed` → `virtum ps` (IP + LAN) → `virtum shell web` → `ping <IP-db>`.

### Mounts v0.4 (clôture)

Validation manuelle du partage hôte ↔ VM :

```bash
virtum doctor
virtum plan -f examples/Virtumfile.mounts
virtum up -f examples/Virtumfile.mounts
virtum shell web    # ls ~/app ; echo test > ~/app/foo  → visible dans examples/backend/
virtum down -f examples/Virtumfile.mounts --destroy --purge
```

Voir [`STEPS4.md`](./STEPS4.md) §7 et [`SPEC_TECH_V0.4.md`](./SPEC_TECH_V0.4.md).

**Lecture seule par défaut** : un partage est en lecture seule pour la VM, imposé côté hôte (`virtiofsd --readonly` à partir de virtiofsd 1.11, sinon virtiofsd lancé sous bubblewrap avec le dossier monté en lecture seule). Pour que la VM puisse écrire : `rw: true` (forme détaillée) ou suffixe `:rw` (forme courte `./out:~/out:rw`). Changer l'accès d'un partage existant : `virtum destroy` puis `virtum up`. Un partage `rw` ne peut pas contenir le Virtumfile, le playbook ni le fichier `requirements`.

**Prérequis mounts** : paquet `virtiofsd`, et `bubblewrap` si virtiofsd est antérieur à 1.11 (Ubuntu 24.04 : 1.10) ; sans `uidmap` (`newuidmap`), Virtum lance virtiofsd avec `--sandbox=none` (isolation réduite — voir `virtum doctor`).

### Banc de test v0.9

Sur des labs jetables :

```bash
time virtum up -f Virtumfile               # 3 VM : en parallèle, récapitulatif par machine
virtum up -f Virtumfile                    # second up : rien ne change
virtum up -f Virtumfile --parallel 1       # (lab neuf) une machine après l'autre, sortie v0.8
virtum destroy <machine sans ip:>          # puis :
virtum up -f Virtumfile                    # recréée en ~15 s, même adresse
virtum plan -f Virtumfile                  # « Réseau lab : … (NAT …, calculé|déclaré) »
# subnet: sur un réseau déjà utilisé (ex. celui du Wi-Fi) → plan / up refusent et proposent un sous-réseau libre
virtum down -f Virtumfile --destroy
```

Voir [`SPEC_TECH_V0.9.md`](./SPEC_TECH_V0.9.md) §Validation, § 7 et § 8.

### Banc de test v0.8

Lab Kubernetes d'exemple (≈ 10 Go de RAM libres, Ansible et accès internet requis) :

```bash
virtum up -f examples/Virtumfile-kubernetes       # VM + playbook + instantané provisioned
virtum inventory -f examples/Virtumfile-kubernetes -o /tmp/k8s.ini
ansible control_plane -i /tmp/k8s.ini -b -a "kubectl --kubeconfig /etc/kubernetes/admin.conf get nodes"
virtum up -f examples/Virtumfile-kubernetes       # « déjà provisionné »
virtum provision -f examples/Virtumfile-kubernetes -- --tags verify
# casser un nœud (ex. systemctl disable --now kubelet sur node-1), puis :
virtum snapshot restore provisioned -f examples/Virtumfile-kubernetes
virtum down -f examples/Virtumfile-kubernetes --destroy
```

Voir [`SPEC_TECH_V0.8.md`](./SPEC_TECH_V0.8.md) §Scénarios d'acceptation.

### Banc de test v0.7

Sur un lab jetable (pas un lab de travail) :

```bash
virtum up -f Virtumfile                           # sans ip: : DHCP, MAC stable
virsh setmem <vm-id> 1G --config                  # changement hors de Virtum
virtum plan -f Virtumfile                         # « ram: … (modifié hors de Virtum) »
virtum apply -f Virtumfile                        # rétablit la RAM du Virtumfile
# ip: 192.168.<octet>.10 sur une machine (sous-réseau affiché par plan)
virtum plan -f Virtumfile && virtum apply -f Virtumfile   # réservation + redémarrage
virsh net-dumpxml virtum-lab-<name> | grep host  # réservation présente
# retirer une machine du Virtumfile
virtum plan -f Virtumfile                         # « - remove … »
virtum apply --prune -f Virtumfile                # confirmation, puis suppression
virtum down -f Virtumfile --destroy
```

Voir [`SPEC_TECH_V0.7.md`](./SPEC_TECH_V0.7.md) §Validation et §Limites connues.

### Banc de test v0.6

Sur un lab jetable (pas un lab de travail) :

```bash
virtum up -f Virtumfile
virtum snapshot create avant -f Virtumfile        # toutes les VM du lab
virtum shell <vm>                                 # modifier la VM (fichier témoin…)
virtum snapshot restore avant -f Virtumfile       # le témoin a disparu
virtum snapshot ls -f Virtumfile
virtum snapshot rm avant -f Virtumfile
# ssh_port : déplacer sshd sur 2222 dans la VM, déclarer ssh_port: 2222, puis down / up / shell
# restart : virsh dominfo <vm-id> | grep Autostart  après up et après stop
virtum down -f Virtumfile --destroy
```

Voir [`SPEC_TECH_V0.6.md`](./SPEC_TECH_V0.6.md) §Validation.

### Sécurité v0.5

- Chemins invité `guest:` validés (whitelist, préfixe `/home/virtum/`).
- SSH : `StrictHostKeyChecking=accept-new` + `instances/<vm-id>/known_hosts` ; ignore `~/.ssh/config` (`-F /dev/null`).
- Flag `--insecure-ssh` sur `shell`, `run`, `apply`, `up` (restaure v0.4) ; `virtum up --yes-lan` (CI sans TTY).
- Findings audit **V-001–V-006** : voir [`SPEC_TECH_V0.5.md`](./SPEC_TECH_V0.5.md), [`STEPS5.md`](./STEPS5.md).

```bash
virtum shell test2 --insecure-ssh
virtum run --os debian-12 --name db --insecure-ssh
virtum up -f Virtumfile --yes-lan --insecure-ssh
```

Validation manuelle : [`specs/001-security-hardening/quickstart.md`](./specs/001-security-hardening/quickstart.md).

## Licence

À définir.
