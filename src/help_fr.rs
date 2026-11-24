//! Aide (`--help`) en français (v1.4).
//!
//! Les textes de l'aide sont écrits en anglais dans `cli.rs` (`///`). En français,
//! [`localize`] remplace, après construction de la commande clap : descriptions des
//! commandes et des options, titres, `-h, --help`, `-V, --version`, la sous-commande `help`,
//! et réécrit valeurs par défaut et valeurs possibles dans le texte de l'option.
//! Un test vérifie que chaque commande et chaque option a sa traduction.

use clap::{Arg, Command};

use crate::i18n::{lang, Lang};

/// Modèle de l'aide : « Utilisation » au lieu de « Usage ».
const TEMPLATE: &str = "{about-with-newline}\nUtilisation : {usage}\n\n{all-args}{after-help}";

const HELP_COMMAND: &str = "Affiche ce message ou l'aide des sous-commandes indiquées";

/// Description de chaque commande, par chemin (`""` = virtum).
const COMMANDS: &[(&str, &str)] = &[
    ("", "Lance des VM locales reproductibles, avec une expérience proche de Docker"),
    ("doctor", "Vérifie que l'hôte est prêt à faire tourner des VM"),
    ("ps", "Liste les VM (comme `docker ps` ; `-a` pour inclure les VM arrêtées)"),
    ("pull", "Télécharge et met en cache une image cloud d'OS (sans lancer de VM)"),
    ("run", "Lance une VM sans affichage"),
    (
        "apply",
        "Applique les changements du Virtumfile aux VM existantes du lab (RAM, CPU, bind, ports, disk_size, mounts, ssh_port, restart, ip)",
    ),
    ("inventory", "Affiche un inventaire Ansible des machines en marche d'un lab"),
    (
        "provision",
        "Rejoue le playbook Ansible du lab (section `ansible:`) ; les arguments après `--` vont à ansible-playbook",
    ),
    ("plan", "Montre ce que ferait le déploiement du lab (création, reprise, mises à jour, avertissements), sans rien changer"),
    ("up", "Déploie toutes les machines d'un Virtumfile (par défaut : ./Virtumfile)"),
    ("open", "Ouvre le pare-feu de l'hôte (ufw) pour les ports `bind: lan` d'un Virtumfile"),
    ("close", "Retire les règles ufw ajoutées par Virtum pour un lab (`virtum open`)"),
    ("down", "Arrête ou supprime toutes les VM d'un lab (`name` du Virtumfile)"),
    ("shell", "Ouvre une session SSH sur une VM en marche"),
    ("start", "Démarre une ou plusieurs VM arrêtées (mêmes instances, sans nouveau provisionnement)"),
    (
        "stop",
        "Arrête une ou plusieurs VM (arrêt propre, puis forcé après 30 s ; `restart: unless-stopped` ne les relance alors plus au démarrage de l'hôte)",
    ),
    (
        "destroy",
        "Supprime une ou plusieurs VM et leurs données d'instance locales (y compris une `vm-…` orpheline sans métadonnées)",
    ),
    ("snapshot", "Instantanés de disque nommés, d'une VM ou de tout un lab (les VM sont arrêtées pendant l'opération)"),
    ("snapshot create", "Prend un instantané nommé (les VM en marche sont arrêtées, puis redémarrées)"),
    ("snapshot restore", "Ramène des VM à un instantané (les VM en marche sont éteintes, puis redémarrées)"),
    ("snapshot ls", "Liste les instantanés"),
    ("snapshot rm", "Supprime un instantané (les VM en marche sont arrêtées, puis redémarrées)"),
    ("config", "Affiche ou modifie les réglages de Virtum (langue de la sortie)"),
    ("config set", "Modifie un réglage (ex. `virtum config set language fr`)"),
    ("config get", "Affiche la valeur d'un réglage et son origine"),
    ("config unset", "Retire un réglage (retour à la valeur par défaut)"),
];

const FILE: &str = "Chemin du Virtumfile";
const INSECURE_SSH: &str =
    "Désactive la vérification de la clé d'hôte SSH (StrictHostKeyChecking=no) ; réservé aux scripts de confiance";
const VM_REF: &str = "Id de la VM (`vm-…`) ou nom (`--name` de `virtum run`)";
const VM_REF_SHORT: &str = "Id ou nom de la VM";
const VM_REF_LAB: &str = "Id ou nom de la VM (par défaut : tout le lab)";
const NO_RESTART: &str = "Laisse les VM arrêtées ensuite";
const KEY: &str = "Nom du réglage";

/// Description de chaque option : (chemin de la commande, id de l'option, texte).
const ARGS: &[(&str, &str, &str)] = &[
    ("ps", "all", "Affiche toutes les instances (y compris arrêtées), pas seulement celles en marche"),
    ("pull", "os", "Image d'OS : debian-13, ubuntu-26.04 (et debian-12, ubuntu-24.04)"),
    ("run", "os", "Image d'OS : debian-13, ubuntu-26.04 (et debian-12, ubuntu-24.04)"),
    ("run", "ram", "RAM en mégaoctets"),
    ("run", "cpu", "Nombre de processeurs virtuels"),
    ("run", "user", "Utilisateur de connexion (cloud-init)"),
    ("run", "hostname", "Nom d'hôte de l'invité ; par défaut `--name` s'il est donné, sinon l'id vm-… généré"),
    ("run", "name", "Nom court pour les commandes (`virtum shell db`) ; doit être unique"),
    (
        "run",
        "disk_size",
        "Taille du disque virtuel racine (ex. `10G`, `20G`) ; agrandi par cloud-init au premier démarrage s'il dépasse l'image de base",
    ),
    ("run", "network", "Réseau de la VM : `default` (NAT libvirt 192.168.122.x) ou `bridge` (LAN physique, macvtap)"),
    ("run", "bridge_interface", "Interface de l'hôte pour `--network bridge` (par défaut : celle de la route par défaut ; voir `virtum doctor`)"),
    ("run", "insecure_ssh", INSECURE_SSH),
    ("apply", "file", FILE),
    ("apply", "insecure_ssh", INSECURE_SSH),
    ("apply", "prune", "Supprime aussi les machines du lab absentes du Virtumfile (VM, disque, instantanés)"),
    ("apply", "yes", "Avec `--prune` : pas de confirmation interactive (scripts / CI)"),
    ("apply", "yes_lan", "Confirme l'exposition `bind: lan` (0.0.0.0) sans question interactive (CI / scripts)"),
    ("inventory", "file", FILE),
    ("inventory", "format", "Format de sortie"),
    ("inventory", "output", "Écrit dans ce fichier (de façon atomique) au lieu de la sortie standard"),
    ("inventory", "insecure_ssh", "Inventaire sans vérification de la clé d'hôte SSH (StrictHostKeyChecking=no, comme --insecure-ssh)"),
    ("provision", "file", FILE),
    ("provision", "insecure_ssh", INSECURE_SSH),
    ("provision", "args", "Arguments transmis à ansible-playbook (ex. `-- --tags init --limit node-1`)"),
    ("plan", "file", FILE),
    ("up", "file", FILE),
    ("up", "yes_lan", "Confirme le déploiement avec `bind: lan` sans question interactive (CI / scripts)"),
    ("up", "insecure_ssh", INSECURE_SSH),
    ("up", "parallel", "Machines créées ou reprises en même temps (1 = l'une après l'autre)"),
    ("open", "file", FILE),
    ("close", "file", FILE),
    ("down", "file", FILE),
    ("down", "destroy", "Supprime les VM et leurs données d'instance (comme destroy), au lieu de seulement les arrêter"),
    ("down", "purge", "Avec `--destroy` : supprime aussi les dossiers de l'hôte déclarés dans `mounts:` (sous le Virtumfile)"),
    ("shell", "vm", VM_REF),
    ("shell", "insecure_ssh", INSECURE_SSH),
    ("start", "vms", VM_REF),
    ("stop", "vms", VM_REF_SHORT),
    ("destroy", "vms", VM_REF_SHORT),
    ("destroy", "purge", "Supprime aussi les dossiers `mounts:` de l'hôte des VM supprimées"),
    ("snapshot create", "name", "Nom de l'instantané (lettres, chiffres, `-`, `_`)"),
    ("snapshot create", "vms", VM_REF_LAB),
    ("snapshot create", "file", FILE),
    ("snapshot create", "no_restart", NO_RESTART),
    ("snapshot restore", "name", "Nom de l'instantané"),
    ("snapshot restore", "vms", VM_REF_LAB),
    ("snapshot restore", "file", FILE),
    ("snapshot restore", "no_restart", NO_RESTART),
    ("snapshot ls", "vms", VM_REF_LAB),
    ("snapshot ls", "file", FILE),
    ("snapshot rm", "name", "Nom de l'instantané"),
    ("snapshot rm", "vms", VM_REF_LAB),
    ("snapshot rm", "file", FILE),
    ("snapshot rm", "no_restart", NO_RESTART),
    ("config set", "key", KEY),
    ("config set", "value", "Nouvelle valeur (language : en, fr)"),
    ("config get", "key", KEY),
    ("config unset", "key", KEY),
];

/// Chemin sans les sous-commandes `help` générées par clap (`help snapshot ls` → `snapshot ls`).
fn logical_path(path: &str) -> String {
    path.split(' ')
        .filter(|s| !s.is_empty() && *s != "help")
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_text(path: &str) -> Option<&'static str> {
    COMMANDS.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
}

fn arg_text(path: &str, id: &str) -> Option<&'static str> {
    ARGS.iter()
        .find(|(p, i, _)| *p == path && *i == id)
        .map(|(_, _, t)| *t)
}

/// Aide traduite si la langue active est le français ; inchangée sinon.
pub fn localize(cmd: Command) -> Command {
    if lang() != Lang::Fr {
        return cmd;
    }
    let mut cmd = cmd;
    // Construite d'abord : clap ajoute `--help`, `--version` et la sous-commande `help`.
    cmd.build();
    localize_command(cmd, "")
}

fn localize_command(mut cmd: Command, path: &str) -> Command {
    let is_help_command = path.split(' ').next_back() == Some("help");
    let text = if is_help_command {
        Some(HELP_COMMAND)
    } else {
        command_text(&logical_path(path))
    };
    if let Some(text) = text {
        cmd = cmd.about(text).long_about(None::<&str>);
    }
    cmd = cmd
        .help_template(TEMPLATE)
        .subcommand_help_heading("Commandes");

    let logical = logical_path(path);
    let ids: Vec<String> = cmd
        .get_arguments()
        .map(|a| a.get_id().to_string())
        .collect();
    for id in ids {
        cmd = cmd.mut_arg(id.as_str(), |arg| localize_arg(arg, &logical, &id));
    }

    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in names {
        let child = if path.is_empty() {
            name.clone()
        } else {
            format!("{path} {name}")
        };
        cmd = cmd.mut_subcommand(name.as_str(), |sub| localize_command(sub, &child));
    }
    cmd
}

fn localize_arg(arg: Arg, path: &str, id: &str) -> Arg {
    match id {
        "help" => {
            let more = arg
                .get_help()
                .is_some_and(|h| h.to_string().contains("--help"));
            let text = if more {
                "Affiche l'aide (plus de détails avec « --help »)"
            } else {
                "Affiche l'aide"
            };
            // Variante longue (`--help`, `virtum help …`), quand clap en génère une.
            let long = arg
                .get_long_help()
                .map(|_| "Affiche l'aide (résumé avec « -h »)");
            return arg.help(text).long_help(long);
        }
        "version" => return arg.help("Affiche la version"),
        _ => {}
    }
    let Some(base) = arg_text(path, id) else {
        return arg;
    };
    let mut text = base.to_string();
    let takes_value = arg.get_action().takes_values();
    let values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|v| !v.is_hide_set())
        .map(|v| v.get_name().to_string())
        .collect();
    if takes_value && !values.is_empty() {
        text.push_str(&format!(" (valeurs : {})", values.join(", ")));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    if takes_value && !defaults.is_empty() && !arg.is_hide_default_value_set() {
        text.push_str(&format!(" (défaut : {})", defaults.join(", ")));
    }
    arg.help(text)
        .long_help(None::<&str>)
        .hide_possible_values(true)
        .hide_default_value(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::with_lang;
    use clap::CommandFactory;

    /// Chaque commande et chaque option (hors `help`/`version` générées) a sa traduction,
    /// et chaque traduction correspond à une commande ou une option qui existe encore.
    #[test]
    fn every_command_and_option_has_a_french_text() {
        let mut cmd = crate::cli::Cli::command();
        cmd.build();
        let mut missing = Vec::new();
        let mut seen_cmds = Vec::new();
        let mut seen_args = Vec::new();
        fn walk(
            cmd: &Command,
            path: &str,
            missing: &mut Vec<String>,
            seen_cmds: &mut Vec<String>,
            seen_args: &mut Vec<(String, String)>,
        ) {
            if path.split(' ').any(|s| s == "help") {
                return;
            }
            seen_cmds.push(path.to_string());
            if command_text(path).is_none() {
                missing.push(format!("commande « {path} »"));
            }
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                if id == "help" || id == "version" {
                    continue;
                }
                seen_args.push((path.to_string(), id.to_string()));
                if arg_text(path, id).is_none() {
                    missing.push(format!("option « {path} » {id}"));
                }
            }
            for sub in cmd.get_subcommands() {
                let child = if path.is_empty() {
                    sub.get_name().to_string()
                } else {
                    format!("{path} {}", sub.get_name())
                };
                walk(sub, &child, missing, seen_cmds, seen_args);
            }
        }
        walk(&cmd, "", &mut missing, &mut seen_cmds, &mut seen_args);
        assert!(missing.is_empty(), "Traduction manquante : {missing:#?}");
        for (p, _) in COMMANDS {
            assert!(
                seen_cmds.iter().any(|s| s == p),
                "commande inconnue dans COMMANDS : « {p} »"
            );
        }
        for (p, i, _) in ARGS {
            assert!(
                seen_args.iter().any(|(sp, si)| sp == p && si == i),
                "option inconnue dans ARGS : « {p} » {i}"
            );
        }
    }

    fn render(path: &[&str], lang: Lang) -> String {
        with_lang(lang, || {
            let mut cmd = localize(crate::cli::Cli::command());
            for name in path {
                cmd = cmd.find_subcommand(name).unwrap().clone();
            }
            cmd.render_help().to_string()
        })
    }

    #[test]
    fn french_help_is_translated() {
        let root = render(&[], Lang::Fr);
        assert!(root.contains("Utilisation : virtum <COMMAND>"), "{root}");
        assert!(root.contains("Commandes:"), "{root}");
        assert!(root.contains("Vérifie que l'hôte est prêt"), "{root}");
        assert!(root.contains("Affiche la version"), "{root}");
        assert!(!root.contains("Usage:"), "{root}");
        let run = render(&["run"], Lang::Fr);
        assert!(run.contains("RAM en mégaoctets (défaut : 2048)"), "{run}");
        assert!(
            run.contains("(valeurs : default, bridge) (défaut : default)"),
            "{run}"
        );
        assert!(!run.contains("[default:"), "{run}");
        assert!(!run.contains("possible values"), "{run}");
        let set = with_lang(Lang::Fr, || {
            let mut cmd = localize(crate::cli::Cli::command());
            let cfg = cmd.find_subcommand_mut("config").unwrap();
            cfg.find_subcommand_mut("set")
                .unwrap()
                .render_long_help()
                .to_string()
        });
        assert!(set.contains("Affiche l'aide (résumé avec « -h »)"), "{set}");
        assert!(!set.contains("Print help"), "{set}");
    }

    #[test]
    fn english_help_is_unchanged() {
        let root = render(&[], Lang::En);
        assert!(root.contains("Usage: virtum <COMMAND>"), "{root}");
        assert!(root.contains("Check that the host is ready"), "{root}");
    }
}
