# Sécurité de Virtum

Ce document décrit contre qui Virtum protège, ce qu'il garantit, et ce qu'il ne peut pas garantir. Chaque garantie est rattachée au code qui la porte et aux tests qui la vérifient : une régression doit faire échouer un test.

## Modèle de menace

Virtum est une CLI lancée par un utilisateur de l'hôte. Elle pilote `qemu:///system` (libvirt système), lance `virtiofsd`, `socat`, `ssh` et `ansible-playbook` sous cet utilisateur.

| Acteur | Ce qu'il contrôle | Confiance |
| :--- | :--- | :--- |
| **L'invité d'une VM** (adversaire principal) | Tout ce qui tourne dans la VM, en root : ses réponses DHCP (nom d'hôte, client-id), l'agent invité, l'ARP, sa clé SSH, le contenu des partages qu'on lui donne en écriture | Aucune |
| **Une autre VM du réseau de lab** | Le trafic du réseau de lab, ses propres baux DHCP | Aucune |
| **Un autre compte Unix de l'hôte**, ou un autre Virtum sur le même libvirt | Ses propres domaines, réseaux et fichiers ; ce que libvirt lui laisse voir | Aucune |
| **Le Virtumfile et le playbook** | Ce que Virtum crée, et le code Ansible exécuté sur l'hôte | Totale : c'est du code de l'utilisateur |
| **L'utilisateur, root, libvirt, QEMU, le noyau** | Tout | Totale |

## Garanties

| # | Garantie | Où |
| :--- | :--- | :--- |
| **G1** | Rien de ce qu'un invité peut modifier n'est exécuté sur l'hôte. Les partages sont **en lecture seule par défaut**, imposée côté hôte (`virtiofsd --readonly`, ou virtiofsd sous bubblewrap avec le dossier monté en lecture seule). `ansible-playbook` tourne sur une **copie privée** du dossier du playbook, d'où les partages inscriptibles (`rw`) sont exclus, liens symboliques compris. Un partage `rw` ne peut pas contenir le Virtumfile, le playbook ni le fichier `requirements`. | `mounts.rs` (`start_virtiofsd`), `ansible.rs` (`snapshot_project`), `virtumfile.rs` (`ensure_host_code_outside_shares`) |
| **G2** | Ce que fournit l'invité ne sert pas à l'identifier. Les baux DHCP sont lus par colonnes (MAC, IP) ; nom d'hôte et client-id sont ignorés. La clé d'hôte SSH est **épinglée par VM** (`HostKeyAlias=<id de la VM>`, `known_hosts` de l'instance) : une autre VM qui obtient l'IP présente une autre clé et la connexion est refusée. | `libvirt.rs` (`parse_dhcp_lease_line`), `ssh.rs` (`SshSessionOpts`) |
| **G3** | Virtum ne touche qu'à ses propres ressources. Domaines et réseaux portent un marqueur de propriétaire (métadonnées libvirt) ; un objet marqué par un autre Virtum n'est ni démarré, ni arrêté, ni redéfini, ni supprimé. Un objet sans marqueur n'est touché que s'il utilise le disque de l'instance. Un signal n'est envoyé qu'à un processus de l'utilisateur dont la ligne de commande correspond. | `ownership.rs`, `libvirt.rs` (`refuse_foreign_domain`), `lab_network.rs`, `procs.rs` |
| **G4** | Les fichiers d'état ne suivent pas de lien symbolique : écriture atomique (`O_EXCL | O_NOFOLLOW` puis `rename`), relecture seulement d'un fichier régulier de l'utilisateur. Clé privée en 0600, répertoire des clés en 0700. La racine d'un partage est fermée aux autres comptes (un invité peut y créer un exécutable setuid de l'utilisateur). | `fsutil.rs`, `ssh.rs`, `mounts.rs` (`restrict_share_root`) |
| **G5** | Le Virtumfile est validé avant usage : noms (lab, machines, volumes, groupes, instantanés), chemins hôte relatifs et sous le Virtumfile, chemins invité sous `/home/virtum`, partages non imbriqués. Les valeurs passées à `virsh`, `ssh` et cloud-init le sont en arguments séparés, sans shell. | `virtumfile.rs`, `mounts.rs`, `cloud_init.rs` |
| **G6** | Chaque lab a son réseau libvirt isolé (`virtum-lab-<nom>`). Les `ports:` n'écoutent que sur `127.0.0.1`, sauf `bind: lan` explicite (confirmé). | `lab_network.rs`, `port_forward.rs` |
| **G7** | Aucun secret n'est affiché ni écrit dans les journaux : la clé privée n'est jamais lue par Virtum, seulement passée par chemin à `ssh`. | `ssh.rs` |

## Limites

Ce que Virtum ne protège pas, et ce qu'il faut faire à la place :

- **Accéder à `qemu:///system`, c'est presque être root.** Être membre du groupe `libvirt` permet de créer une VM qui monte n'importe quel disque de l'hôte. Ne mettez dans ce groupe que des comptes de confiance.
- **Un Virtumfile est du code.** Lancer le Virtumfile ou le playbook d'un inconnu revient à exécuter son code sur votre machine.
- **Partage `rw`** : la VM peut y écrire n'importe quoi. N'exécutez pas sur l'hôte ce qu'elle y dépose, et ne placez pas dans un partage `rw` un dossier que l'hôte lit (dépôt Git avec hooks, configuration d'un outil…).
- **Ansible** : seul le dossier du playbook est copié. Un fichier chargé par chemin absolu, ou par `../` hors de ce dossier, n'est pas protégé par la copie (un `../` échoue, un chemin absolu vers un partage `rw` serait lu tel quel).
- **Partages créés avant la lecture seule par défaut** : le virtiofsd déjà lancé reste en écriture jusqu'à `virtum destroy` puis `virtum up`.
- **Clés SSH épinglées avant `HostKeyAlias`** : la première connexion à une VM existante après la mise à jour enregistre sa clé (confiance au premier usage).
- **`--insecure-ssh`** désactive la vérification de la clé d'hôte. Réservé aux scripts de confiance.
- **Sans `uidmap`**, virtiofsd tourne avec `--sandbox=none` : une faille de virtiofsd donnerait les droits de l'utilisateur.
- **Failles de QEMU, libvirt, virtiofsd, du noyau ou des images cloud** : hors de portée de Virtum. Tenez l'hôte et les images à jour (`virtum pull`).

## Signaler une faille

Ouvrez une issue privée (security advisory) sur le dépôt GitHub, avec les étapes pour reproduire. Ne publiez pas de détails d'exploitation avant le correctif.
