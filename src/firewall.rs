//! Ouverture / fermeture pare-feu hôte (ufw) pour `bind: lan`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, VirtumPaths};
use crate::tr;
use crate::virtumfile::{self, LanFirewallRule};

const FIREWALL_STATE_PREFIX: &str = "firewall-";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LabFirewallState {
    pub lab_name: String,
    pub virtumfile: String,
    pub rules: Vec<LanFirewallRule>,
    pub opened_at: String,
}

pub fn open(file: Option<PathBuf>) -> Result<()> {
    let virtumfile_path = virtumfile::resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&virtumfile_path)?;
    let rules = virtumfile::collect_lan_firewall_rules(&lab)?;

    if rules.is_empty() {
        bail!(tr!(
            fr: "Aucun port à ouvrir : définissez `bind: lan` et des `ports:` (ex. 18080:80) \
             sur au moins une machine du Virtumfile.",
            en: "No port to open: set `bind: lan` and `ports:` (e.g. 18080:80) \
             on at least one machine of the Virtumfile."
        ));
    }

    ensure_ufw_available()?;

    let commands: Vec<String> = rules
        .iter()
        .map(|r| format!("ufw allow {}/tcp", r.host_port))
        .collect();

    print_sudo_preview(&commands);
    run_sudo_commands(&commands)?;

    let paths = config::ensure_layout_blocking()?;
    save_firewall_state(&paths, &lab.name, &virtumfile_path, &rules)?;

    println!(
        "{}",
        tr!(
            fr: "Pare-feu : {} règle(s) ajoutée(s) pour le lab '{}'.",
            en: "Firewall: {} rule(s) added for lab '{}'.",
            rules.len(),
            lab.name
        )
    );
    for rule in &rules {
        println!(
            "  allow {}/tcp — machine '{}' ({}:{})", // i18n-ok : identique en anglais
            rule.host_port, rule.machine, rule.host_port, rule.guest_port
        );
    }
    println!(
        "{}",
        tr!(
            fr: "Pour retirer ces règles : virtum close -f {}",
            en: "To remove these rules: virtum close -f {}",
            virtumfile_path.display()
        )
    );
    Ok(())
}

pub fn close(file: Option<PathBuf>) -> Result<()> {
    let virtumfile_path = virtumfile::resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&virtumfile_path)?;
    let paths = config::ensure_layout_blocking()?;

    let state = match load_firewall_state(&paths, &lab.name)? {
        Some(s) => s,
        None => {
            let rules = virtumfile::collect_lan_firewall_rules(&lab)?;
            if rules.is_empty() {
                bail!(tr!(
                    fr: "Aucune règle Virtum enregistrée pour le lab '{}' \
                     et aucun port `bind: lan` dans le Virtumfile.",
                    en: "No Virtum rule recorded for lab '{}' \
                     and no `bind: lan` port in the Virtumfile.",
                    lab.name
                ));
            }
            eprintln!(
                "{}",
                tr!(
                    fr: "Note : aucun `virtum open` enregistré pour '{}'. \
                     Fermeture des ports `bind: lan` du Virtumfile \
                     (ex. ouverture manuelle avec ufw).",
                    en: "Note: no `virtum open` recorded for '{}'. \
                     Closing the Virtumfile's `bind: lan` ports \
                     (e.g. opened manually with ufw).",
                    lab.name
                )
            );
            LabFirewallState {
                lab_name: lab.name.clone(),
                virtumfile: virtumfile_path.display().to_string(),
                rules,
                opened_at: String::new(),
            }
        }
    };

    if state.rules.is_empty() {
        let _ = remove_firewall_state(&paths, &lab.name);
        bail!(tr!(
            fr: "État pare-feu vide pour le lab '{}'.",
            en: "Empty firewall state for lab '{}'.",
            lab.name
        ));
    }

    ensure_ufw_available()?;

    let commands: Vec<String> = state
        .rules
        .iter()
        .map(|r| format!("ufw delete allow {}/tcp", r.host_port))
        .collect();

    print_sudo_preview(&commands);
    run_sudo_commands(&commands)?;

    remove_firewall_state(&paths, &lab.name)?;

    println!(
        "{}",
        tr!(
            fr: "Pare-feu : {} règle(s) retirée(s) pour le lab '{}'.",
            en: "Firewall: {} rule(s) removed for lab '{}'.",
            state.rules.len(),
            lab.name
        )
    );
    Ok(())
}

/// État `virtum open` enregistré pour ce lab.
pub fn has_firewall_state(paths: &VirtumPaths, lab_name: &str) -> Result<bool> {
    Ok(load_firewall_state(paths, lab_name)?.is_some())
}

/// Charge l'état pare-feu enregistré par `virtum open` (si présent).
pub fn get_firewall_state(paths: &VirtumPaths, lab_name: &str) -> Result<Option<LabFirewallState>> {
    load_firewall_state(paths, lab_name)
}

/// Ports hôte TCP triés (pour affichage / comparaison).
pub fn lan_host_ports(rules: &[LanFirewallRule]) -> Vec<u16> {
    let mut ports: Vec<u16> = rules.iter().map(|r| r.host_port).collect();
    ports.sort();
    ports
}

/// Affiche des ports hôte au format `18080/tcp`.
pub fn format_ports_tcp(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|p| format!("{p}/tcp"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compare les ports `bind: lan` du Virtumfile et ceux du dernier `virtum open`.
pub fn firewall_ports_aligned(desired: &[LanFirewallRule], stored: &[LanFirewallRule]) -> bool {
    lan_host_ports(desired) == lan_host_ports(stored)
}

/// `ENABLED=yes` dans `/etc/ufw/ufw.conf` (sans exécuter `ufw status`).
pub fn is_ufw_enabled() -> bool {
    ufw_enabled_in_config().unwrap_or(false)
}

/// True si le Virtumfile a des ports en `bind: lan` (pour message après `virtum up`).
pub fn lab_needs_firewall_hint(lab: &virtumfile::Virtumfile) -> bool {
    virtumfile::collect_lan_firewall_rules(lab)
        .map(|rules| !rules.is_empty())
        .unwrap_or(false)
}

fn firewall_state_path(paths: &VirtumPaths, lab_name: &str) -> PathBuf {
    paths
        .root
        .join(format!("{FIREWALL_STATE_PREFIX}{lab_name}.json"))
}

fn save_firewall_state(
    paths: &VirtumPaths,
    lab_name: &str,
    virtumfile_path: &Path,
    rules: &[LanFirewallRule],
) -> Result<()> {
    let state = LabFirewallState {
        lab_name: lab_name.to_string(),
        virtumfile: virtumfile_path.display().to_string(),
        rules: rules.to_vec(),
        opened_at: crate::fsutil::now_rfc3339(),
    };
    let path = firewall_state_path(paths, lab_name);
    let content = serde_json::to_string_pretty(&state)
        .context(tr!(fr: "sérialisation pare-feu", en: "serializing firewall state"))?;
    crate::fsutil::write_atomic(&path, format!("{content}\n").as_bytes(), 0o644)
}

fn load_firewall_state(paths: &VirtumPaths, lab_name: &str) -> Result<Option<LabFirewallState>> {
    let path = firewall_state_path(paths, lab_name);
    let Some(content) = crate::fsutil::read_owned(&path)? else {
        return Ok(None);
    };
    let state: LabFirewallState = serde_json::from_str(&content).with_context(
        || tr!(fr: "État pare-feu invalide: {}", en: "Invalid firewall state: {}", path.display()),
    )?;
    Ok(Some(state))
}

fn remove_firewall_state(paths: &VirtumPaths, lab_name: &str) -> Result<()> {
    let path = firewall_state_path(paths, lab_name);
    if path.is_file() {
        std::fs::remove_file(&path).with_context(
            || tr!(fr: "Impossible de supprimer {}", en: "Cannot delete {}", path.display()),
        )?;
    }
    Ok(())
}

fn ensure_ufw_available() -> Result<()> {
    let which = Command::new("which")
        .arg("ufw")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !which {
        bail!(tr!(
            fr: "ufw est introuvable. Sur Debian/Ubuntu : sudo apt install ufw. \
             Pare-feu firewalld / nftables : ouvrez les ports manuellement.",
            en: "ufw not found. On Debian/Ubuntu: sudo apt install ufw. \
             With a firewalld / nftables firewall: open the ports manually."
        ));
    }

    // `ufw status` exige root et sa sortie dépend de la locale ; lire ufw.conf suffit.
    if !ufw_enabled_in_config()? {
        bail!(tr!(
            fr: "ufw n'est pas activé (ENABLED=no dans /etc/ufw/ufw.conf). \
             Activez-le avec : sudo ufw enable\n\
             Puis relancez : virtum open -f <Virtumfile>",
            en: "ufw is not enabled (ENABLED=no in /etc/ufw/ufw.conf). \
             Enable it with: sudo ufw enable\n\
             Then run again: virtum open -f <Virtumfile>"
        ));
    }
    Ok(())
}

/// Lit `ENABLED=yes` dans `/etc/ufw/ufw.conf` (lisible sans root).
fn ufw_enabled_in_config() -> Result<bool> {
    const UFW_CONF: &str = "/etc/ufw/ufw.conf";
    let content = std::fs::read_to_string(UFW_CONF)
        .with_context(|| tr!(fr: "Impossible de lire {UFW_CONF}", en: "Cannot read {UFW_CONF}"))?;
    Ok(parse_ufw_enabled(&content))
}

fn parse_ufw_enabled(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.split('#').next().unwrap_or(line).trim();
        line.eq_ignore_ascii_case("ENABLED=yes")
    })
}

fn print_sudo_preview(commands: &[String]) {
    eprintln!(
        "{}",
        tr!(
            fr: "Les commandes suivantes seront exécutées avec les droits \
             administrateur (sudo) :\n",
            en: "The following commands will be run with administrator rights (sudo):\n"
        )
    );
    for cmd in commands {
        eprintln!("  {cmd}");
    }
    eprintln!();
    eprintln!(
        "{}",
        tr!(
            fr: "Un mot de passe sudo peut vous être demandé.\n",
            en: "You may be asked for your sudo password.\n"
        )
    );
}

fn run_sudo_commands(commands: &[String]) -> Result<()> {
    if commands.is_empty() {
        return Ok(());
    }
    let script = commands.join(" && ");
    let status = Command::new("sudo")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context(tr!(fr: "Impossible d'exécuter sudo", en: "Cannot run sudo"))?;

    if status.success() {
        return Ok(());
    }
    bail!(tr!(
        fr: "sudo a échoué (code {:?}). Vérifiez ufw et réessayez.",
        en: "sudo failed (code {:?}). Check ufw and try again.",
        status.code()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ufw_enabled_yes_no() {
        let conf = "# comment\nENABLED=yes\n";
        assert!(parse_ufw_enabled(conf));
        assert!(!parse_ufw_enabled("ENABLED=no\n"));
        assert!(parse_ufw_enabled("  enabled=YES  # trailing\n"));
    }

    #[test]
    fn ufw_allow_command_format() {
        let rule = LanFirewallRule {
            machine: "web".into(),
            host_port: 18080,
            guest_port: 80,
        };
        let cmd = format!("ufw allow {}/tcp", rule.host_port);
        assert_eq!(cmd, "ufw allow 18080/tcp");
    }

    fn rule(host: u16, guest: u16) -> LanFirewallRule {
        LanFirewallRule {
            machine: "web".into(),
            host_port: host,
            guest_port: guest,
        }
    }

    #[test]
    fn firewall_ports_aligned_ignores_order_and_guest() {
        let a = vec![rule(18088, 80), rule(2222, 22)];
        let b = vec![rule(2222, 22), rule(18088, 80)];
        assert!(firewall_ports_aligned(&a, &b));
    }

    #[test]
    fn firewall_ports_not_aligned_when_host_port_differs() {
        let desired = vec![rule(18088, 80)];
        let stored = vec![rule(18080, 80)];
        assert!(!firewall_ports_aligned(&desired, &stored));
    }

    #[test]
    fn format_ports_tcp_sorted_display() {
        assert_eq!(format_ports_tcp(&[18088, 2222]), "18088/tcp, 2222/tcp");
    }
}
