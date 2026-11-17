//! Ouverture / fermeture pare-feu hôte (ufw) pour `bind: lan`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, VirtumPaths};
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
        bail!(
            "Aucun port à ouvrir : définissez `bind: lan` et des `ports:` (ex. 18080:80) \
             sur au moins une machine du Virtumfile."
        );
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
        "Pare-feu : {} règle(s) ajoutée(s) pour le lab '{}'.",
        rules.len(),
        lab.name
    );
    for rule in &rules {
        println!(
            "  allow {}/tcp — machine '{}' ({}:{})",
            rule.host_port, rule.machine, rule.host_port, rule.guest_port
        );
    }
    println!(
        "Pour retirer ces règles : virtum close -f {}",
        virtumfile_path.display()
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
                bail!(
                    "Aucune règle Virtum enregistrée pour le lab '{}' et aucun port `bind: lan` dans le Virtumfile.",
                    lab.name
                );
            }
            eprintln!(
                "Note : aucun `virtum open` enregistré pour '{}'. \
                 Fermeture des ports `bind: lan` du Virtumfile (ex. ouverture manuelle avec ufw).",
                lab.name
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
        bail!("État pare-feu vide pour le lab '{}'.", lab.name);
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
        "Pare-feu : {} règle(s) retirée(s) pour le lab '{}'.",
        state.rules.len(),
        lab.name
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
    let content = serde_json::to_string_pretty(&state).context("sérialisation pare-feu")?;
    crate::fsutil::write_atomic(&path, format!("{content}\n").as_bytes(), 0o644)
}

fn load_firewall_state(paths: &VirtumPaths, lab_name: &str) -> Result<Option<LabFirewallState>> {
    let path = firewall_state_path(paths, lab_name);
    let Some(content) = crate::fsutil::read_owned(&path)? else {
        return Ok(None);
    };
    let state: LabFirewallState = serde_json::from_str(&content)
        .with_context(|| format!("État pare-feu invalide: {}", path.display()))?;
    Ok(Some(state))
}

fn remove_firewall_state(paths: &VirtumPaths, lab_name: &str) -> Result<()> {
    let path = firewall_state_path(paths, lab_name);
    if path.is_file() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Impossible de supprimer {}", path.display()))?;
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
        bail!(
            "ufw est introuvable. Sur Debian/Ubuntu : sudo apt install ufw. \
             Pare-feu firewalld / nftables : ouvrez les ports manuellement."
        );
    }

    // `ufw status` exige root et sa sortie dépend de la locale ; lire ufw.conf suffit.
    if !ufw_enabled_in_config()? {
        bail!(
            "ufw n'est pas activé (ENABLED=no dans /etc/ufw/ufw.conf). \
             Activez-le avec : sudo ufw enable\n\
             Puis relancez : virtum open -f <Virtumfile>"
        );
    }
    Ok(())
}

/// Lit `ENABLED=yes` dans `/etc/ufw/ufw.conf` (lisible sans root).
fn ufw_enabled_in_config() -> Result<bool> {
    const UFW_CONF: &str = "/etc/ufw/ufw.conf";
    let content = std::fs::read_to_string(UFW_CONF)
        .with_context(|| format!("Impossible de lire {UFW_CONF}"))?;
    Ok(parse_ufw_enabled(&content))
}

fn parse_ufw_enabled(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.split('#').next().unwrap_or(line).trim();
        line.eq_ignore_ascii_case("ENABLED=yes")
    })
}

fn print_sudo_preview(commands: &[String]) {
    eprintln!("Les commandes suivantes seront exécutées avec les droits administrateur (sudo) :\n");
    for cmd in commands {
        eprintln!("  {cmd}");
    }
    eprintln!();
    eprintln!("Un mot de passe sudo peut vous être demandé.\n");
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
        .context("Impossible d'exécuter sudo")?;

    if status.success() {
        return Ok(());
    }
    bail!(
        "sudo a échoué (code {:?}). Vérifiez ufw et réessayez.",
        status.code()
    );
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
