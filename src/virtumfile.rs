//! Parse and validate `Virtumfile` (Virtum v0.2 lab-as-code, v0.3 réseau).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::bridge_network;
use crate::disk::parse_disk_size;
use crate::images::resolve_image;
use crate::metadata::validate_name;
use crate::port_forward::{self, PortBind, PortForward};

pub const DEFAULT_VIRTUMFILE: &str = "Virtumfile";

/// Port hôte à ouvrir dans ufw pour `bind: lan`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanFirewallRule {
    pub machine: String,
    pub host_port: u16,
    pub guest_port: u16,
}

/// Résout le chemin du Virtumfile (comme `virtum up` / `virtum down`).
pub fn resolve_virtumfile_path(file: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    use std::path::PathBuf;
    let path = file.unwrap_or_else(|| PathBuf::from(DEFAULT_VIRTUMFILE));
    if path.is_file() {
        return Ok(path);
    }
    bail!(
        "Virtumfile introuvable: {}. \
         Lancez la commande depuis le répertoire du lab ou utilisez `-f /chemin/Virtumfile`.",
        path.display()
    );
}

/// Ports à exposer sur le pare-feu hôte (`bind: lan` + `ports:`).
pub fn collect_lan_firewall_rules(lab: &Virtumfile) -> Result<Vec<LanFirewallRule>> {
    let mut rules = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (machine_name, spec) in &lab.machines {
        if effective_port_bind(lab, machine_name) != PortBind::Lan {
            continue;
        }
        for port in &spec.ports {
            let (host_port, guest_port) = parse_port_mapping(port)
                .with_context(|| format!("Machine '{machine_name}': port invalide"))?;
            if seen.insert(host_port) {
                rules.push(LanFirewallRule {
                    machine: machine_name.clone(),
                    host_port,
                    guest_port,
                });
            }
        }
    }
    rules.sort_by_key(|r| r.host_port);
    Ok(rules)
}

/// Mode réseau Virtumfile / `virtum run` (v0.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Réseau lab isolé `virtum-lab-<name>` (défaut).
    Lab,
    /// NIC sur le LAN physique (bridge / macvtap, étapes 38+).
    Bridge,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Virtumfile {
    pub name: String,
    /// Réseau par défaut pour les machines sans surcharge.
    #[serde(default)]
    pub network: Option<NetworkMode>,
    #[serde(default)]
    pub bridge_interface: Option<String>,
    /// Écoute des `ports:` : `local` (127.0.0.1) ou `lan` (0.0.0.0, même réseau Wi‑Fi/Ethernet).
    #[serde(default)]
    pub bind: Option<PortBind>,
    pub machines: BTreeMap<String, MachineSpec>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MachineSpec {
    pub os: String,
    #[serde(default = "default_ram")]
    pub ram: u32,
    #[serde(default = "default_cpu")]
    pub cpu: u32,
    #[serde(default)]
    pub disk_size: Option<String>,
    #[serde(default)]
    pub network: Option<NetworkMode>,
    #[serde(default)]
    pub bridge_interface: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub bind: Option<PortBind>,
    #[serde(default)]
    pub provision: Vec<String>,
}

fn default_ram() -> u32 {
    2048
}

fn default_cpu() -> u32 {
    2
}

/// Mode réseau effectif : machine > lab > `lab`.
pub fn effective_network_mode(lab: &Virtumfile, machine_name: &str) -> NetworkMode {
    let spec = &lab.machines[machine_name];
    spec.network.or(lab.network).unwrap_or(NetworkMode::Lab)
}

/// Interface bridge explicite : machine > lab.
pub fn effective_bridge_interface<'a>(lab: &'a Virtumfile, machine_name: &str) -> Option<&'a str> {
    let spec = &lab.machines[machine_name];
    spec.bridge_interface
        .as_deref()
        .or(lab.bridge_interface.as_deref())
}

/// Au moins une machine (ou le lab entier) requiert le LAN physique.
pub fn lab_uses_bridge(lab: &Virtumfile) -> bool {
    if lab.network == Some(NetworkMode::Bridge) {
        return true;
    }
    lab.machines
        .keys()
        .any(|m| effective_network_mode(lab, m) == NetworkMode::Bridge)
}

/// Lab en mode mixte : au moins une machine `bridge` et une autre en `lab`.
pub fn lab_has_bridge_lab_mix(lab: &Virtumfile) -> bool {
    let mut has_bridge = false;
    let mut has_lab = false;
    for name in lab.machines.keys() {
        match effective_network_mode(lab, name) {
            NetworkMode::Bridge => has_bridge = true,
            NetworkMode::Lab => has_lab = true,
        }
    }
    has_bridge && has_lab
}

/// Le lab doit créer / utiliser `virtum-lab-<name>` (NAT isolé).
pub fn lab_needs_isolated_network(lab: &Virtumfile) -> bool {
    if lab.network == Some(NetworkMode::Bridge) {
        return false;
    }
    for name in lab.machines.keys() {
        if effective_network_mode(lab, name) == NetworkMode::Lab {
            return true;
        }
    }
    lab_has_bridge_lab_mix(lab)
}

/// Réseau libvirt + macvtap pour une machine de lab (`virtum up`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineNetworkConfig {
    pub mode: NetworkMode,
    /// Réseau `virtum-lab-*` si NIC lab (ou dual NIC hybride).
    pub libvirt_network: Option<String>,
    /// Interface hôte macvtap si NIC LAN.
    pub bridge_host_interface: Option<String>,
}

pub fn resolve_machine_network(
    lab: &Virtumfile,
    machine_name: &str,
    lab_net_name: Option<&str>,
) -> Result<MachineNetworkConfig> {
    let mode = effective_network_mode(lab, machine_name);
    let bridge_host_interface = if mode == NetworkMode::Bridge {
        let explicit = effective_bridge_interface(lab, machine_name);
        Some(bridge_network::resolve_bridge_interface(explicit)?)
    } else {
        None
    };
    let needs_lab_nic =
        mode == NetworkMode::Lab || (mode == NetworkMode::Bridge && lab_has_bridge_lab_mix(lab));
    let libvirt_network = if needs_lab_nic {
        let name = lab_net_name.with_context(|| {
            format!(
                "Réseau lab requis pour la machine '{machine_name}' mais `virtum-lab-*` n'a pas été créé."
            )
        })?;
        Some(name.to_string())
    } else {
        None
    };
    Ok(MachineNetworkConfig {
        mode,
        libvirt_network,
        bridge_host_interface,
    })
}

/// `bind` effectif : machine > lab > `local`.
pub fn effective_port_bind(lab: &Virtumfile, machine_name: &str) -> PortBind {
    let spec = &lab.machines[machine_name];
    spec.bind.or(lab.bind).unwrap_or(PortBind::Local)
}

/// Interface explicite à valider en priorité (lab puis machines en bridge).
pub fn lab_bridge_interface_for_validation(lab: &Virtumfile) -> Option<&str> {
    if lab.network == Some(NetworkMode::Bridge) {
        return lab.bridge_interface.as_deref();
    }
    for name in lab.machines.keys() {
        if effective_network_mode(lab, name) == NetworkMode::Bridge {
            if let Some(iface) = effective_bridge_interface(lab, name) {
                return Some(iface);
            }
        }
    }
    lab.bridge_interface.as_deref()
}

/// Charge et valide un Virtumfile depuis le disque.
pub fn load(path: &Path) -> Result<Virtumfile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
    load_from_str(&content).with_context(|| format!("Virtumfile invalide: {}", path.display()))
}

pub fn load_from_str(content: &str) -> Result<Virtumfile> {
    let lab: Virtumfile = serde_yaml::from_str(content)
        .context("YAML invalide (vérifiez `network:` — valeurs acceptées : lab, bridge)")?;
    validate(&lab)?;
    Ok(lab)
}

pub fn validate(lab: &Virtumfile) -> Result<()> {
    validate_name(&lab.name).context("Champ `name` (nom du lab) invalide")?;
    if lab.machines.is_empty() {
        bail!("Le Virtumfile doit définir au moins une machine sous `machines`.");
    }
    if let Some(ref iface) = lab.bridge_interface {
        validate_bridge_interface_name(iface)?;
    }

    let mut port_machines: Vec<(String, Vec<PortForward>)> = Vec::new();
    for (machine_name, spec) in &lab.machines {
        validate_name(machine_name)
            .with_context(|| format!("Nom de machine '{machine_name}' invalide"))?;
        if let Some(ref iface) = spec.bridge_interface {
            validate_bridge_interface_name(iface)
                .with_context(|| format!("Machine '{machine_name}': bridge_interface invalide"))?;
        }
        resolve_image(&spec.os, Path::new("/tmp/virtum-validate-images"))
            .with_context(|| format!("Machine '{machine_name}': os inconnu"))?;
        if spec.ram == 0 {
            bail!("Machine '{machine_name}': `ram` doit être > 0.");
        }
        if spec.cpu == 0 {
            bail!("Machine '{machine_name}': `cpu` doit être > 0.");
        }
        if let Some(ref size) = spec.disk_size {
            parse_disk_size(size)
                .with_context(|| format!("Machine '{machine_name}': disk_size invalide"))?;
        }
        let mut forwards = Vec::new();
        for port in &spec.ports {
            let (host_port, guest_port) = parse_port_mapping(port)
                .with_context(|| format!("Machine '{machine_name}': port invalide"))?;
            forwards.push(PortForward {
                host_port,
                guest_port,
            });
        }
        port_machines.push((machine_name.clone(), forwards));
    }
    port_forward::validate_lab_port_uniqueness(&port_machines)?;

    if lab_uses_bridge(lab) {
        let explicit = lab_bridge_interface_for_validation(lab);
        bridge_network::require_bridge_interface(explicit).with_context(|| {
            "Réseau `bridge` demandé mais l'hôte n'est pas prêt. Lancez `virtum doctor`."
        })?;
    }

    Ok(())
}

fn validate_bridge_interface_name(iface: &str) -> Result<()> {
    if iface.trim().is_empty() {
        bail!("`bridge_interface` ne peut pas être vide.");
    }
    Ok(())
}

/// Parse les `ports:` d'une machine Virtumfile.
pub fn machine_port_forwards(spec: &MachineSpec) -> Result<Vec<PortForward>> {
    spec.ports
        .iter()
        .map(|port| {
            let (host_port, guest_port) = parse_port_mapping(port)?;
            Ok(PortForward {
                host_port,
                guest_port,
            })
        })
        .collect()
}

/// Parse `8080:80` (hôte:invité).
pub fn parse_port_mapping(input: &str) -> Result<(u16, u16)> {
    let s = input.trim();
    let (host, guest) = s
        .split_once(':')
        .with_context(|| format!("Port invalide '{input}' (attendu host:guest, ex. 8080:80)"))?;
    let host_port: u16 = host
        .parse()
        .with_context(|| format!("Port hôte invalide dans '{input}'"))?;
    let guest_port: u16 = guest
        .parse()
        .with_context(|| format!("Port invité invalide dans '{input}'"))?;
    if host_port == 0 || guest_port == 0 {
        bail!("Les ports doivent être > 0 dans '{input}'.");
    }
    Ok((host_port, guest_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name: web-stack-lab

machines:
  web:
    os: debian-12
    ram: 2048
    cpu: 2
    disk_size: 10G
    provision:
      - apt-get update
      - apt-get install -y nginx

  db:
    os: debian-12
    ram: 1024
    cpu: 1
"#;

    fn parse_lab(content: &str) -> Virtumfile {
        serde_yaml::from_str(content).expect("YAML parse")
    }

    #[test]
    fn loads_valid_lab() {
        let lab = load_from_str(SAMPLE).unwrap();
        assert_eq!(lab.name, "web-stack-lab");
        assert_eq!(lab.machines.len(), 2);
        assert_eq!(lab.machines["web"].disk_size.as_deref(), Some("10G"));
        assert_eq!(lab.network, None);
        assert_eq!(effective_network_mode(&lab, "web"), NetworkMode::Lab);
        assert_eq!(effective_network_mode(&lab, "db"), NetworkMode::Lab);
    }

    #[test]
    fn rejects_unknown_os() {
        let yaml = r#"
name: x
machines:
  a:
    os: arch-linux
"#;
        assert!(load_from_str(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_network_value() {
        let yaml = r#"
name: x
network: foo
machines:
  a:
    os: debian-12
"#;
        assert!(load_from_str(yaml).is_err());
    }

    #[test]
    fn lab_global_bridge_inheritance() {
        let yaml = r#"
name: demo-lan
network: bridge
machines:
  api:
    os: debian-12
"#;
        let lab = parse_lab(yaml);
        assert_eq!(effective_network_mode(&lab, "api"), NetworkMode::Bridge);
    }

    #[test]
    fn machine_overrides_lab_network() {
        let yaml = r#"
name: hybrid
network: lab
machines:
  web:
    os: debian-12
    network: bridge
  db:
    os: debian-12
"#;
        let lab = parse_lab(yaml);
        assert_eq!(effective_network_mode(&lab, "web"), NetworkMode::Bridge);
        assert_eq!(effective_network_mode(&lab, "db"), NetworkMode::Lab);
        assert!(lab_has_bridge_lab_mix(&lab));
    }

    #[test]
    fn bridge_interface_inheritance_machine_over_lab() {
        let yaml = r#"
name: x
network: bridge
bridge_interface: enp0s0
machines:
  web:
    os: debian-12
    network: bridge
    bridge_interface: enp3s0
  db:
    os: debian-12
"#;
        let lab = parse_lab(yaml);
        assert_eq!(effective_bridge_interface(&lab, "web"), Some("enp3s0"));
        assert_eq!(effective_bridge_interface(&lab, "db"), Some("enp0s0"));
    }

    #[test]
    fn lab_needs_isolated_network_cases() {
        let nat = parse_lab(
            r#"
name: n
machines:
  a:
    os: debian-12
"#,
        );
        assert!(lab_needs_isolated_network(&nat));

        let all_bridge = parse_lab(
            r#"
name: b
network: bridge
machines:
  a:
    os: debian-12
"#,
        );
        assert!(!lab_needs_isolated_network(&all_bridge));

        let mix = parse_lab(
            r#"
name: m
machines:
  web:
    os: debian-12
    network: bridge
  db:
    os: debian-12
"#,
        );
        assert!(lab_needs_isolated_network(&mix));
    }

    #[test]
    fn lab_uses_bridge_detects_per_machine_only() {
        let yaml = r#"
name: x
machines:
  web:
    os: debian-12
    network: bridge
  db:
    os: debian-12
"#;
        let lab = parse_lab(yaml);
        assert!(lab_uses_bridge(&lab));
        assert!(lab_has_bridge_lab_mix(&lab));
    }

    #[test]
    fn collect_lan_firewall_rules_skips_local_bind() {
        let yaml = r#"
name: x
bind: lan
machines:
  web:
    os: debian-12
    ports:
      - "18080:80"
  db:
    os: debian-12
"#;
        let lab = parse_lab(yaml);
        let rules = collect_lan_firewall_rules(&lab).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].host_port, 18080);
        assert_eq!(rules[0].machine, "web");
    }

    #[test]
    fn lab_bind_lan_inheritance() {
        let yaml = r#"
name: x
bind: lan
machines:
  web:
    os: debian-12
    ports:
      - "18080:80"
  db:
    os: debian-12
"#;
        let lab = parse_lab(yaml);
        assert_eq!(effective_port_bind(&lab, "web"), PortBind::Lan);
        assert_eq!(effective_port_bind(&lab, "db"), PortBind::Lan);
    }

    #[test]
    fn machine_bind_overrides_lab() {
        let yaml = r#"
name: x
bind: lan
machines:
  web:
    os: debian-12
    bind: local
"#;
        let lab = parse_lab(yaml);
        assert_eq!(effective_port_bind(&lab, "web"), PortBind::Local);
    }

    #[test]
    fn accepts_ports_in_virtumfile() {
        let yaml = r#"
name: x
machines:
  web:
    os: debian-12
    ports:
      - "18080:80"
"#;
        let lab = load_from_str(yaml).unwrap();
        let forwards = machine_port_forwards(&lab.machines["web"]).unwrap();
        assert_eq!(forwards[0].host_port, 18080);
    }

    #[test]
    fn rejects_duplicate_host_ports_in_lab() {
        let yaml = r#"
name: x
machines:
  web:
    os: debian-12
    ports:
      - "18080:80"
  api:
    os: debian-12
    ports:
      - "18080:8080"
"#;
        assert!(load_from_str(yaml).is_err());
    }

    #[test]
    fn parse_port_mapping_ok() {
        assert_eq!(parse_port_mapping("8080:80").unwrap(), (8080, 80));
    }

    #[test]
    fn rejects_nonexistent_bridge_interface() {
        let yaml = r#"
name: x
network: bridge
bridge_interface: nonexistent_iface_xyz_999
machines:
  a:
    os: debian-12
"#;
        let err = load_from_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("introuvable") || msg.contains("nonexistent_iface"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn rejects_empty_bridge_interface() {
        let yaml = r#"
name: x
bridge_interface: "   "
machines:
  a:
    os: debian-12
"#;
        assert!(load_from_str(yaml).is_err());
    }
}
