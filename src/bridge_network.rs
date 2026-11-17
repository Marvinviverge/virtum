//! Détection d'interface physique pour le réseau bridge (Virtum v0.3, STEPS3 §37).

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Capacité de l'hôte à exposer des VM sur le LAN via bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCapability {
    /// Interface recommandée pour `network: bridge` (Ethernet de préférence).
    pub recommended_interface: Option<String>,
    /// Interfaces Ethernet candidates (hors virtuelles évidentes).
    pub ethernet_interfaces: Vec<String>,
    /// Avertissements non bloquants (Wi‑Fi, etc.).
    pub warnings: Vec<String>,
}

/// Analyse l'hôte sans exiger qu'une interface soit utilisable tout de suite.
pub fn scan_bridge_capability() -> BridgeCapability {
    let mut warnings = Vec::new();
    let names = match list_sys_net_interfaces() {
        Ok(n) => n,
        Err(e) => {
            warnings.push(format!("Impossible de lire /sys/class/net: {e}"));
            return BridgeCapability {
                recommended_interface: None,
                ethernet_interfaces: Vec::new(),
                warnings,
            };
        }
    };

    let mut ethernet = Vec::new();
    let mut wireless = Vec::new();

    for name in names {
        if !is_plausible_bridge_candidate(&name) {
            continue;
        }
        if is_wireless(&name) {
            wireless.push(name);
        } else if is_ethernet_type(&name) {
            ethernet.push(name);
        }
    }

    ethernet.sort();
    wireless.sort();

    let recommended = pick_recommended_interface(&ethernet, &wireless);

    if recommended.is_none() && ethernet.is_empty() && wireless.is_empty() {
        warnings.push(
            "Aucune interface physique détectée (Ethernet ou Wi‑Fi). \
             Branchez un câble Ethernet ou définissez `bridge_interface:` dans le Virtumfile."
                .to_string(),
        );
    } else if let Some(ref iface) = recommended {
        if is_wireless(iface) {
            warnings.push(format!(
                "Interface recommandée « {iface} » est du Wi‑Fi : le bridge LAN est souvent \
                 instable ou indisponible. Préférez Ethernet (câble) pour les démos et la formation."
            ));
        }
    }

    BridgeCapability {
        recommended_interface: recommended,
        ethernet_interfaces: ethernet,
        warnings,
    }
}

/// Résout l'interface bridge : explicite ou auto-détection.
pub fn resolve_bridge_interface(explicit: Option<&str>) -> Result<String> {
    if let Some(name) = explicit {
        return validate_explicit_interface(name);
    }

    let cap = scan_bridge_capability();
    for w in &cap.warnings {
        eprintln!("Avertissement bridge: {w}");
    }

    cap.recommended_interface.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Aucune interface adaptée au bridge LAN. Branchez un Ethernet ou indiquez \
             `bridge_interface:` dans le Virtumfile, puis lancez `virtum doctor`."
        )
    })
}

/// Vérifie qu'un bridge est possible (Virtumfile / CLI).
pub fn require_bridge_interface(explicit: Option<&str>) -> Result<()> {
    resolve_bridge_interface(explicit).map(|_| ())
}

fn validate_explicit_interface(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("`bridge_interface` ne peut pas être vide.");
    }
    let path = Path::new("/sys/class/net").join(name);
    if !path.exists() {
        let hint = default_route_interface()
            .map(|d| format!(" Route par défaut actuelle : `{d}` (ex. `--bridge-interface {d}`)."))
            .unwrap_or_default();
        let cap = scan_bridge_capability();
        let candidates =
            if cap.ethernet_interfaces.is_empty() && cap.recommended_interface.is_none() {
                String::new()
            } else {
                let mut list: Vec<String> = cap.ethernet_interfaces.clone();
                if let Some(ref r) = cap.recommended_interface {
                    if !list.iter().any(|i| i == r) {
                        list.insert(0, r.clone());
                    }
                }
                format!(" Interfaces détectées : {}.", list.join(", "))
            };
        bail!(
            "Interface réseau « {name} » introuvable sur cet hôte.{hint}{candidates} \
             Vérifiez `ip -br link` ou `virtum doctor`."
        );
    }
    if name == "lo" {
        bail!("L'interface loopback « lo » ne peut pas servir de bridge LAN.");
    }
    if !is_plausible_bridge_candidate(name) {
        bail!(
            "L'interface « {name} » semble virtuelle ou gérée par Virtum/libvirt ; \
             choisissez l'interface physique du bureau (ex. enp3s0, eth0)."
        );
    }
    if is_wireless(name) {
        eprintln!(
            "Avertissement: « {name} » est du Wi‑Fi — le bridge LAN peut échouer ; Ethernet recommandé."
        );
    }
    Ok(name.to_string())
}

fn list_sys_net_interfaces() -> Result<Vec<String>> {
    let dir = Path::new("/sys/class/net");
    let entries = fs::read_dir(dir).with_context(|| format!("Lecture de {}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        names.push(name);
    }
    Ok(names)
}

/// Interface utilisable pour un bridge vers le LAN (filtre docker/virbr/veth…).
pub fn is_plausible_bridge_candidate(name: &str) -> bool {
    if name == "lo" {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    const PREFIX_SKIP: &[&str] = &[
        "docker", "br-", "virbr", "veth", "vnet", "vmnet", "tun", "tap", "wg", "vb-",
    ];
    !PREFIX_SKIP.iter().any(|p| lower.starts_with(p))
}

fn is_ethernet_type(name: &str) -> bool {
    let path = Path::new("/sys/class/net").join(name).join("type");
    match fs::read_to_string(path) {
        Ok(s) => s.trim() == "1",
        Err(_) => true,
    }
}

fn is_wireless(name: &str) -> bool {
    Path::new("/sys/class/net")
        .join(name)
        .join("wireless")
        .exists()
}

/// IPv4 source de la route par défaut (IP du PC sur le LAN / partage actif).
pub fn host_primary_ipv4() -> Option<String> {
    let output = Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_default_route_src(&String::from_utf8_lossy(&output.stdout))
}

fn parse_default_route_src(route_output: &str) -> Option<String> {
    for line in route_output.lines() {
        let line = line.trim();
        if !line.starts_with("default ") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(idx) = parts.iter().position(|&t| t == "src") {
            return parts.get(idx + 1).map(|s| s.to_string());
        }
    }
    None
}

/// Interface portant la route par défaut IPv4 (`ip -4 route show default`).
pub fn default_route_interface() -> Option<String> {
    let output = Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_default_route_device(&String::from_utf8_lossy(&output.stdout))
}

fn parse_default_route_device(route_output: &str) -> Option<String> {
    for line in route_output.lines() {
        let line = line.trim();
        if !line.starts_with("default ") {
            continue;
        }
        let mut parts = line.split_whitespace();
        while let Some(tok) = parts.next() {
            if tok == "dev" {
                return parts.next().map(str::to_string);
            }
        }
    }
    None
}

fn interface_oper_up(name: &str) -> bool {
    let state = Path::new("/sys/class/net").join(name).join("operstate");
    fs::read_to_string(state)
        .map(|s| {
            let s = s.trim().to_ascii_lowercase();
            s == "up" || s == "unknown"
        })
        .unwrap_or(false)
}

fn interface_has_ipv4(name: &str) -> bool {
    let output = Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", name])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(" inet ") && !line.contains(" 127."))
}

fn pick_recommended_interface(ethernet: &[String], wireless: &[String]) -> Option<String> {
    if let Some(dev) = default_route_interface() {
        if is_plausible_bridge_candidate(&dev) && interface_oper_up(&dev) {
            return Some(dev);
        }
    }

    if let Some(name) = ethernet
        .iter()
        .chain(wireless.iter())
        .find(|n| interface_oper_up(n) && interface_has_ipv4(n))
    {
        return Some(name.clone());
    }

    ethernet
        .first()
        .cloned()
        .or_else(|| wireless.first().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_virtual_interface_names() {
        assert!(!is_plausible_bridge_candidate("lo"));
        assert!(!is_plausible_bridge_candidate("virbr0"));
        assert!(!is_plausible_bridge_candidate("docker0"));
        assert!(!is_plausible_bridge_candidate("veth123"));
        assert!(!is_plausible_bridge_candidate("vmnet1"));
        assert!(is_plausible_bridge_candidate("enp3s0"));
        assert!(is_plausible_bridge_candidate("eth0"));
        assert!(is_plausible_bridge_candidate("wlan0"));
    }

    #[test]
    fn rejects_loopback_explicit() {
        assert!(validate_explicit_interface("lo").is_err());
    }

    #[test]
    fn parses_default_route_device() {
        let sample = "default via 192.168.43.1 dev wlp3s0 proto dhcp src 192.168.43.2\n";
        assert_eq!(
            parse_default_route_device(sample).as_deref(),
            Some("wlp3s0")
        );
    }
}
