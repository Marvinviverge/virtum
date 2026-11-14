//! Parse and validate `Virtumfile` (Virtum v0.2 lab-as-code, v0.3 réseau).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::bridge_network;
use crate::disk::parse_disk_size;
use crate::images::resolve_image;
use crate::metadata::validate_name;
use crate::mounts::{self, parse_mount_entries};
use crate::port_forward::{self, PortBind, PortForward};
use serde::Deserializer;

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
    /// Dossiers partagés hérités par toutes les machines (v0.4 virtiofs).
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_mounts_field")]
    pub mounts: Option<Vec<MountSpec>>,
    /// Port sshd une fois la machine configurée (défaut 22), hérité par toutes les machines.
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// Démarrage au boot de l'hôte (`no`, `always`, `unless-stopped`), hérité par les machines.
    #[serde(default)]
    pub restart: Option<RestartPolicy>,
    /// Provisionnement Ansible du lab depuis l'hôte (v0.8).
    #[serde(default)]
    pub ansible: Option<AnsibleSpec>,
    /// Sous-réseau du réseau isolé du lab (`a.b.c.0/24`, v0.9) ; absent = calculé depuis le nom.
    #[serde(default)]
    pub subnet: Option<String>,
    pub machines: BTreeMap<String, MachineSpec>,
}

/// Section `ansible:` du Virtumfile.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnsibleSpec {
    /// Playbook, relatif au répertoire du Virtumfile.
    pub playbook: String,
    /// Fichier requirements (collections / rôles) pour `ansible-galaxy`.
    #[serde(default)]
    pub requirements: Option<String>,
    /// Instantané de lab après le premier provisionnement : nom, `true` (défaut) ou `false`.
    #[serde(default)]
    pub snapshot: Option<SnapshotSetting>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SnapshotSetting {
    Enabled(bool),
    Name(String),
}

/// Nom d'instantané par défaut après provisionnement.
pub const DEFAULT_PROVISION_SNAPSHOT: &str = "provisioned";

impl AnsibleSpec {
    /// Nom de l'instantané à prendre, `None` si désactivé.
    pub fn snapshot_name(&self) -> Option<String> {
        match &self.snapshot {
            None | Some(SnapshotSetting::Enabled(true)) => {
                Some(DEFAULT_PROVISION_SNAPSHOT.to_string())
            }
            Some(SnapshotSetting::Enabled(false)) => None,
            Some(SnapshotSetting::Name(name)) => Some(name.clone()),
        }
    }
}

/// Chemin d'un fichier de la section `ansible:` : relatif, sous le répertoire du
/// Virtumfile (liens résolus), fichier existant.
pub fn resolve_ansible_path(
    virtumfile_dir: &Path,
    field: &str,
    value: &str,
) -> Result<std::path::PathBuf> {
    let rel = Path::new(value.trim());
    if value.trim().is_empty() || rel.is_absolute() {
        bail!("ansible.{field} : chemin relatif au Virtumfile attendu ('{value}').");
    }
    let base = virtumfile_dir.canonicalize().with_context(|| {
        format!(
            "Répertoire du Virtumfile introuvable : {}",
            virtumfile_dir.display()
        )
    })?;
    let full = base.join(rel);
    let resolved = full
        .canonicalize()
        .with_context(|| format!("ansible.{field} : fichier introuvable ({})", full.display()))?;
    if !resolved.starts_with(&base) {
        bail!("ansible.{field} : '{value}' sort du répertoire du Virtumfile.");
    }
    if !resolved.is_file() {
        bail!("ansible.{field} : '{value}' n'est pas un fichier.");
    }
    Ok(resolved)
}

fn validate_ansible(lab: &Virtumfile, virtumfile_dir: &Path) -> Result<()> {
    let Some(ref spec) = lab.ansible else {
        return Ok(());
    };
    resolve_ansible_path(virtumfile_dir, "playbook", &spec.playbook)?;
    if let Some(ref req) = spec.requirements {
        resolve_ansible_path(virtumfile_dir, "requirements", req)?;
    }
    if let Some(name) = spec.snapshot_name() {
        crate::snapshot::validate_snapshot_name(&name).context("ansible.snapshot invalide")?;
    }
    Ok(())
}

/// Un partage inscriptible (`rw: true`) ne doit pas contenir ce que l'hôte lit pour
/// piloter le lab : le Virtumfile, le playbook, le fichier `requirements`.
///
/// Ce contrôle donne une erreur claire tôt. La protection elle-même est ailleurs :
/// `ansible-playbook` tourne sur une copie privée du projet d'où les partages
/// inscriptibles sont exclus (voir `ansible::snapshot_project`). Les partages en lecture
/// seule ne sont pas concernés : l'invité ne peut rien y écrire.
pub(crate) fn ensure_host_code_outside_shares(
    lab: &Virtumfile,
    virtumfile_dir: &Path,
    writable_shares: &[(String, std::path::PathBuf)],
) -> Result<()> {
    if writable_shares.is_empty() {
        return Ok(());
    }
    let base = virtumfile_dir.canonicalize().with_context(|| {
        format!(
            "Répertoire du Virtumfile introuvable : {}",
            virtumfile_dir.display()
        )
    })?;
    let mut protected = vec![(base.clone(), "le Virtumfile".to_string())];
    if let Some(ref spec) = lab.ansible {
        let playbook = resolve_ansible_path(virtumfile_dir, "playbook", &spec.playbook)?;
        protected.push((playbook, "le playbook".to_string()));
        if let Some(ref req) = spec.requirements {
            let req = resolve_ansible_path(virtumfile_dir, "requirements", req)?;
            protected.push((req, "ansible.requirements".to_string()));
        }
    }
    for (label, host) in writable_shares {
        let host = host.canonicalize().unwrap_or_else(|_| host.clone());
        for (path, what) in &protected {
            if path.starts_with(&host) {
                bail!(
                    "Mount '{label}' ({}) est inscriptible (rw) et contient {what} ({}) : \
                     l'invité pourrait modifier ce que l'hôte exécute. Partagez un sous-dossier \
                     qui ne contient ni le Virtumfile ni les fichiers Ansible, ou retirez `rw`.",
                    host.display(),
                    path.display()
                );
            }
        }
    }
    Ok(())
}

/// Volume virtiofs déclaré dans le Virtumfile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountSpec {
    pub name: String,
    pub host: String,
    pub guest: String,
    /// Partage inscriptible par l'invité (`rw: true` ou suffixe `:rw`). Par défaut, un
    /// partage est en lecture seule, imposée côté hôte.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rw: bool,
}

fn deserialize_mounts_field<'de, D>(deserializer: D) -> Result<Option<Vec<MountSpec>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<Vec<serde_yaml::Value>> = Option::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(entries) => parse_mount_entries(&entries)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
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
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_mounts_field")]
    pub mounts: Option<Vec<MountSpec>>,
    /// Port sshd de cette machine (surcharge `ssh_port` du lab).
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// Démarrage au boot de l'hôte pour cette machine (surcharge `restart` du lab).
    #[serde(default)]
    pub restart: Option<RestartPolicy>,
    /// Adresse déclarée sur le réseau du lab (réservation DHCP) ; absente = DHCP libre.
    #[serde(default)]
    pub ip: Option<String>,
    /// Groupes Ansible de la machine (`virtum inventory`, v0.8).
    #[serde(default)]
    pub groups: Vec<String>,
}

/// Démarrage automatique au boot de l'hôte (`restart:`, comme docker compose).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
pub enum RestartPolicy {
    /// Jamais démarrée automatiquement.
    #[serde(rename = "no")]
    No,
    /// Démarrée au boot, même après `virtum stop` / `down`.
    #[serde(rename = "always")]
    Always,
    /// Démarrée au boot sauf si arrêtée volontairement (`virtum stop` / `down`).
    #[serde(rename = "unless-stopped")]
    UnlessStopped,
}

impl RestartPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Always => "always",
            Self::UnlessStopped => "unless-stopped",
        }
    }

    /// Autostart libvirt voulu selon que la VM tourne (`running`) ou a été arrêtée.
    pub fn autostart(self, running: bool) -> bool {
        match self {
            Self::No => false,
            Self::Always => true,
            Self::UnlessStopped => running,
        }
    }
}

/// Mounts effectifs pour une machine (lab + surcharge machine).
pub fn effective_mount_specs(lab: &Virtumfile, machine_name: &str) -> Vec<MountSpec> {
    mounts::effective_mount_specs(lab, machine_name)
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

/// La machine a une carte sur le réseau `virtum-lab-*` (mode `lab` ou hybride).
pub fn machine_has_lab_nic(lab: &Virtumfile, machine_name: &str) -> bool {
    match effective_network_mode(lab, machine_name) {
        NetworkMode::Lab => lab.network != Some(NetworkMode::Bridge),
        NetworkMode::Bridge => lab_has_bridge_lab_mix(lab),
    }
}

/// Adresse `ip:` déclarée (déjà validée par [`validate`]).
pub fn machine_static_ip(spec: &MachineSpec) -> Option<std::net::Ipv4Addr> {
    spec.ip.as_deref().and_then(|ip| ip.trim().parse().ok())
}

/// Règles statiques de `ip:` : IPv4, réseau du lab, pas réservée, unique, carte lab présente.
fn validate_static_ips(lab: &Virtumfile) -> Result<()> {
    let mut seen: BTreeMap<std::net::Ipv4Addr, &str> = BTreeMap::new();
    for (machine_name, spec) in &lab.machines {
        let Some(raw) = spec.ip.as_deref() else {
            continue;
        };
        let ip: std::net::Ipv4Addr = raw.trim().parse().map_err(|_| {
            anyhow::anyhow!("Machine '{machine_name}' : ip: '{raw}' n'est pas une adresse IPv4.")
        })?;
        if !machine_has_lab_nic(lab, machine_name) {
            bail!(
                "Machine '{machine_name}' : `ip:` n'est possible que sur le réseau du lab. \
                 En `network: bridge`, l'adresse vient du DHCP du LAN, que Virtum ne contrôle pas."
            );
        }
        crate::lab_network::validate_lab_ip(crate::lab_network::lab_subnet(lab), ip)
            .with_context(|| format!("Machine '{machine_name}' : ip: {ip} invalide"))?;
        if let Some(other) = seen.insert(ip, machine_name) {
            bail!("ip: {ip} est déclarée pour '{other}' et '{machine_name}'.");
        }
    }
    Ok(())
}

/// `subnet:` : `a.b.c.0/24` privé, et seulement si le lab a un réseau isolé.
fn validate_subnet(lab: &Virtumfile) -> Result<()> {
    let Some(ref value) = lab.subnet else {
        return Ok(());
    };
    crate::lab_network::LabSubnet::parse(value)?;
    if !lab_needs_isolated_network(lab) {
        bail!(
            "`subnet:` est sans effet : le lab n'a pas de réseau isolé (toutes les machines en `network: bridge`)."
        );
    }
    Ok(())
}

/// Groupe Ansible implicite du lab : nom du lab, `-` → `_` (préfixe `lab_` si besoin).
pub fn lab_group_name(lab_name: &str) -> String {
    let name = lab_name.trim().replace('-', "_");
    if is_ansible_group_name(&name) {
        name
    } else {
        format!("lab_{name}")
    }
}

fn is_ansible_group_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Règles de `groups:` : identifiants Ansible, ni `all` / `ungrouped`, pas de doublon,
/// pas de groupe homonyme d'une machine ni du groupe implicite du lab… sauf s'il s'agit
/// du même groupe (déclarer le groupe du lab est inutile mais accepté).
fn validate_groups(lab: &Virtumfile) -> Result<()> {
    for (machine_name, spec) in &lab.machines {
        let mut seen = std::collections::BTreeSet::new();
        for group in &spec.groups {
            if !is_ansible_group_name(group) {
                bail!(
                    "Machine '{machine_name}' : groupe '{group}' invalide — lettres, chiffres et `_`, \
                     sans commencer par un chiffre (Ansible refuse `-`)."
                );
            }
            if group == "all" || group == "ungrouped" {
                bail!("Machine '{machine_name}' : '{group}' est un groupe réservé d'Ansible.");
            }
            if lab.machines.contains_key(group.as_str()) {
                bail!(
                    "Machine '{machine_name}' : le groupe '{group}' porte le nom d'une machine du lab."
                );
            }
            if !seen.insert(group.as_str()) {
                bail!("Machine '{machine_name}' : groupe '{group}' en double.");
            }
        }
    }
    Ok(())
}

/// Port SSH standard (image cloud fraîche).
pub const DEFAULT_SSH_PORT: u16 = 22;

/// `ssh_port` effectif : machine > lab > 22.
pub fn effective_ssh_port(lab: &Virtumfile, machine_name: &str) -> u16 {
    let spec = &lab.machines[machine_name];
    spec.ssh_port.or(lab.ssh_port).unwrap_or(DEFAULT_SSH_PORT)
}

/// `restart` effectif : machine > lab ; `None` = non déclaré (autostart libvirt laissé tel quel).
pub fn effective_restart(lab: &Virtumfile, machine_name: &str) -> Option<RestartPolicy> {
    let spec = &lab.machines[machine_name];
    spec.restart.or(lab.restart)
}

/// True si `virtum up` doit demander confirmation (bind: lan + au moins un port mappé).
pub fn lab_requires_lan_consent(lab: &Virtumfile) -> Result<bool> {
    for (machine_name, spec) in &lab.machines {
        if effective_port_bind(lab, machine_name) != PortBind::Lan {
            continue;
        }
        if !spec.ports.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
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
    let lab = load_from_str(&content)
        .with_context(|| format!("Virtumfile invalide: {}", path.display()))?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    validate(&lab, dir)?;
    Ok(lab)
}

/// Répertoire parent du Virtumfile (racine des chemins `host:`).
pub fn virtumfile_dir(path: &Path) -> std::path::PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

pub fn load_from_str(content: &str) -> Result<Virtumfile> {
    let lab: Virtumfile = serde_yaml::from_str(content)
        .context("YAML invalide (vérifiez `network:` — valeurs acceptées : lab, bridge)")?;
    Ok(lab)
}

pub fn validate(lab: &Virtumfile, virtumfile_dir: &Path) -> Result<()> {
    validate_name(&lab.name).context("Champ `name` (nom du lab) invalide")?;
    if lab.machines.is_empty() {
        bail!("Le Virtumfile doit définir au moins une machine sous `machines`.");
    }
    if let Some(ref iface) = lab.bridge_interface {
        validate_bridge_interface_name(iface)?;
    }
    if lab.ssh_port == Some(0) {
        bail!("`ssh_port` doit être compris entre 1 et 65535.");
    }

    let mut port_machines: Vec<(String, Vec<PortForward>)> = Vec::new();
    let mut lab_mounts: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut writable_mounts: Vec<(String, std::path::PathBuf)> = Vec::new();
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
        if spec.ssh_port == Some(0) {
            bail!("Machine '{machine_name}': `ssh_port` doit être compris entre 1 et 65535.");
        }
        let autostarts =
            effective_restart(lab, machine_name).is_some_and(|r| r != RestartPolicy::No);
        if autostarts && !effective_mount_specs(lab, machine_name).is_empty() {
            bail!(
                "Machine '{machine_name}': `restart` est incompatible avec `mounts:` — au boot de l'hôte, \
                 libvirt démarrerait la VM sans virtiofsd. Utilisez `restart: no` ou retirez les mounts."
            );
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
        let resolved = mounts::resolve_machine_mounts(lab, machine_name, virtumfile_dir)
            .with_context(|| format!("Machine '{machine_name}' : mounts invalides"))?;
        for m in resolved {
            let label = format!("{machine_name}/{}", m.name);
            if !m.read_only {
                writable_mounts.push((label.clone(), m.host_abs.clone()));
            }
            lab_mounts.push((label, m.host_abs));
        }
    }
    // Entre machines aussi : le partage d'une machine ne doit pas contenir celui d'une autre.
    mounts::ensure_mounts_not_nested(&lab_mounts)?;
    port_forward::validate_lab_port_uniqueness(&port_machines)?;
    validate_subnet(lab)?;
    validate_static_ips(lab)?;
    validate_groups(lab)?;
    validate_ansible(lab, virtumfile_dir)?;
    ensure_host_code_outside_shares(lab, virtumfile_dir, &writable_mounts)?;

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
        validate(&lab, Path::new("/tmp")).unwrap();
        assert_eq!(lab.name, "web-stack-lab");
        assert_eq!(lab.machines.len(), 2);
        assert_eq!(lab.machines["web"].disk_size.as_deref(), Some("10G"));
        assert_eq!(lab.network, None);
        assert_eq!(effective_network_mode(&lab, "web"), NetworkMode::Lab);
        assert_eq!(effective_network_mode(&lab, "db"), NetworkMode::Lab);
    }

    #[test]
    fn ssh_port_defaults_to_22_and_inherits_from_lab() {
        let lab = load_from_str(SAMPLE).unwrap();
        assert_eq!(effective_ssh_port(&lab, "web"), 22);

        let yaml = r#"
name: infra
ssh_port: 2222
machines:
  app:
    os: ubuntu-24.04
  edge:
    os: ubuntu-24.04
    ssh_port: 2200
"#;
        let lab = load_from_str(yaml).unwrap();
        validate(&lab, Path::new("/tmp")).unwrap();
        assert_eq!(effective_ssh_port(&lab, "app"), 2222);
        assert_eq!(effective_ssh_port(&lab, "edge"), 2200);
    }

    #[test]
    fn restart_policy_parses_and_inherits() {
        let lab = load_from_str(SAMPLE).unwrap();
        assert_eq!(effective_restart(&lab, "web"), None);

        let yaml = r#"
name: infra
restart: unless-stopped
machines:
  app:
    os: ubuntu-24.04
  edge:
    os: ubuntu-24.04
    restart: always
  scratch:
    os: ubuntu-24.04
    restart: no
"#;
        let lab = load_from_str(yaml).unwrap();
        validate(&lab, Path::new("/tmp")).unwrap();
        assert_eq!(
            effective_restart(&lab, "app"),
            Some(RestartPolicy::UnlessStopped)
        );
        assert_eq!(effective_restart(&lab, "edge"), Some(RestartPolicy::Always));
        assert_eq!(effective_restart(&lab, "scratch"), Some(RestartPolicy::No));
        assert!(load_from_str(
            "name: x\nrestart: on-failure\nmachines:\n  a:\n    os: debian-12\n"
        )
        .is_err());
    }

    #[test]
    fn restart_policy_autostart_rules() {
        assert!(!RestartPolicy::No.autostart(true));
        assert!(RestartPolicy::Always.autostart(false));
        assert!(RestartPolicy::UnlessStopped.autostart(true));
        assert!(!RestartPolicy::UnlessStopped.autostart(false));
    }

    #[test]
    fn rejects_restart_with_mounts() {
        let yaml = r#"
name: x
restart: always
machines:
  a:
    os: debian-12
    mounts:
      - ./app:/home/virtum/app
"#;
        let lab = load_from_str(yaml).unwrap();
        let err = validate(&lab, Path::new("/tmp")).unwrap_err();
        assert!(format!("{err:#}").contains("incompatible avec `mounts:`"));
    }

    #[test]
    fn rejects_invalid_ssh_port() {
        let zero = load_from_str("name: x\nmachines:\n  a:\n    os: debian-12\n    ssh_port: 0\n")
            .unwrap();
        assert!(validate(&zero, Path::new("/tmp")).is_err());
        assert!(
            load_from_str("name: x\nssh_port: 70000\nmachines:\n  a:\n    os: debian-12\n")
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_os() {
        let yaml = r#"
name: x
machines:
  a:
    os: arch-linux
"#;
        let lab = load_from_str(yaml).unwrap();
        assert!(validate(&lab, Path::new("/tmp")).is_err());
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
        validate(&lab, Path::new("/tmp")).unwrap();
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
        let lab = load_from_str(yaml).unwrap();
        assert!(validate(&lab, Path::new("/tmp")).is_err());
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
        let lab = load_from_str(yaml).unwrap();
        let err = validate(&lab, Path::new("/tmp")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("introuvable") || msg.contains("nonexistent_iface"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn parses_mounts_shortcut_and_inheritance() {
        let dir = std::env::temp_dir().join(format!("virtum-vf-mounts-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("backend")).unwrap();
        std::fs::create_dir_all(dir.join("common")).unwrap();
        let yaml = r#"
name: m
mounts:
  - ./common:~/shared
machines:
  web:
    os: debian-12
    mounts:
      - name: app
        host: ./backend
        guest: ~/app
"#;
        let lab = load_from_str(yaml).unwrap();
        validate(&lab, &dir).unwrap();
        let eff = effective_mount_specs(&lab, "web");
        assert_eq!(eff.len(), 2);
        assert!(eff.iter().any(|m| m.name == "app"));
        assert!(eff.iter().any(|m| m.name == "common"));
        let _ = std::fs::remove_dir_all(dir);
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
        let lab = load_from_str(yaml).unwrap();
        assert!(validate(&lab, Path::new("/tmp")).is_err());
    }

    #[test]
    fn static_ip_validation() {
        let octet = crate::lab_network::lab_subnet_third_octet("k8s");
        let lab_with = |ips: &[(&str, &str)], extra: &str| {
            let mut yaml = format!("name: k8s\n{extra}machines:\n");
            for (m, ip) in ips {
                yaml.push_str(&format!("  {m}:\n    os: debian-12\n    ip: {ip}\n"));
            }
            load_from_str(&yaml).unwrap()
        };
        let dir = Path::new(".");
        let ok = lab_with(
            &[
                ("cp", &format!("192.168.{octet}.10")),
                ("w1", &format!("192.168.{octet}.11")),
            ],
            "",
        );
        validate(&ok, dir).unwrap();
        assert_eq!(
            machine_static_ip(&ok.machines["cp"]),
            Some(std::net::Ipv4Addr::new(192, 168, octet, 10))
        );

        let not_ip = lab_with(&[("cp", "192.168.1")], "");
        assert!(format!("{:#}", validate(&not_ip, dir).unwrap_err()).contains("IPv4"));

        let gateway = lab_with(&[("cp", &format!("192.168.{octet}.1"))], "");
        assert!(format!("{:#}", validate(&gateway, dir).unwrap_err()).contains("passerelle"));

        let dup = lab_with(
            &[
                ("cp", &format!("192.168.{octet}.10")),
                ("w1", &format!("192.168.{octet}.10")),
            ],
            "",
        );
        assert!(format!("{:#}", validate(&dup, dir).unwrap_err()).contains("'cp' et 'w1'"));

        let outside = lab_with(&[("cp", "10.0.0.5")], "");
        assert!(format!("{:#}", validate(&outside, dir).unwrap_err()).contains("hors du réseau"));
    }

    #[test]
    fn static_ip_needs_lab_nic() {
        let lab =
            load_from_str("name: k8s\nnetwork: bridge\nmachines:\n  cp:\n    os: debian-12\n")
                .unwrap();
        assert!(!machine_has_lab_nic(&lab, "cp"));
        let mixed = load_from_str(
            "name: k8s\nmachines:\n  cp:\n    os: debian-12\n    network: bridge\n  db:\n    os: debian-12\n",
        )
        .unwrap();
        assert!(machine_has_lab_nic(&mixed, "cp"));
        assert!(machine_has_lab_nic(&mixed, "db"));
        assert!(validate_static_ips(&lab).is_ok());
        let mut with_ip = lab.clone();
        with_ip.machines.get_mut("cp").unwrap().ip = Some("192.168.100.10".into());
        assert!(format!("{:#}", validate_static_ips(&with_ip).unwrap_err()).contains("bridge"));
    }

    #[test]
    fn groups_validation_and_lab_group() {
        assert_eq!(lab_group_name("kubernetes-lab"), "kubernetes_lab");
        assert_eq!(lab_group_name("1lab"), "lab_1lab");
        let load = |groups: &str| {
            load_from_str(&format!(
                "name: k8s\nmachines:\n  cp:\n    os: debian-12\n    groups: {groups}\n  w1:\n    os: debian-12\n"
            ))
            .unwrap()
        };
        assert!(validate_groups(&load("[control_plane, k8s_cluster]")).is_ok());
        assert_eq!(load("[a, b]").machines["cp"].groups, vec!["a", "b"]);
        assert!(
            load_from_str("name: k8s\nmachines:\n  cp:\n    os: debian-12\n")
                .unwrap()
                .machines["cp"]
                .groups
                .is_empty()
        );
        for (bad, needle) in [
            ("[control-plane]", "invalide"),
            ("[1st]", "invalide"),
            ("[all]", "réservé"),
            ("[w1]", "nom d'une machine"),
            ("[a, a]", "en double"),
        ] {
            let err = validate_groups(&load(bad)).unwrap_err().to_string();
            assert!(err.contains(needle), "{bad}: {err}");
        }
    }

    #[test]
    fn writable_shares_must_not_contain_virtumfile_or_ansible_files() {
        let dir = std::env::temp_dir().join(format!("virtum-share-ansible-{}", std::process::id()));
        for d in ["ansible/roles", "src", "data"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("ansible/site.yml"), "---\n").unwrap();
        std::fs::write(dir.join("req.yml"), "---\n").unwrap();
        let lab = |host: &str, rw: bool, ansible: bool| {
            let ansible = if ansible {
                "ansible:\n  playbook: ansible/site.yml\n  requirements: req.yml\n"
            } else {
                ""
            };
            load_from_str(&format!(
                "name: s\n{ansible}machines:\n  web:\n    os: debian-12\n    restart: no\n    \
                 mounts:\n      - name: code\n        host: {host}\n        guest: ~/code\n        \
                 rw: {rw}\n"
            ))
            .unwrap()
        };
        // Lecture seule : l'invité ne peut rien écrire, tout partage est accepté.
        for host in [".", "./ansible", "./ansible/roles"] {
            validate(&lab(host, false, true), &dir).unwrap();
        }
        // Inscriptible sans fichier lu par l'hôte : accepté. Un dossier inscriptible dans
        // celui du playbook est accepté aussi : il est exclu de la copie privée d'Ansible.
        validate(&lab("./src", true, true), &dir).unwrap();
        validate(&lab("./data", true, false), &dir).unwrap();
        validate(&lab("./ansible/roles", true, true), &dir).unwrap();
        for (host, ansible, needle) in [
            (".", false, "le Virtumfile"),
            (".", true, "le Virtumfile"),
            ("./ansible", true, "le playbook"),
        ] {
            let err = format!(
                "{:#}",
                validate(&lab(host, true, ansible), &dir).unwrap_err()
            );
            assert!(err.contains(needle), "{host}: {err}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ansible_section_validation() {
        let dir = std::env::temp_dir().join(format!("virtum-ansible-vf-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pb")).unwrap();
        std::fs::write(dir.join("pb/site.yml"), "---\n").unwrap();
        std::fs::write(dir.join("pb/req.yml"), "---\n").unwrap();
        let lab = |ansible: &str| {
            load_from_str(&format!(
                "name: k8s\nansible:\n{ansible}machines:\n  cp:\n    os: debian-12\n"
            ))
            .unwrap()
        };
        let ok = lab("  playbook: pb/site.yml\n  requirements: pb/req.yml\n");
        validate(&ok, &dir).unwrap();
        assert_eq!(
            ok.ansible.as_ref().unwrap().snapshot_name().as_deref(),
            Some("provisioned")
        );
        assert_eq!(
            lab("  playbook: pb/site.yml\n  snapshot: false\n")
                .ansible
                .unwrap()
                .snapshot_name(),
            None
        );
        assert_eq!(
            lab("  playbook: pb/site.yml\n  snapshot: cluster-ok\n")
                .ansible
                .unwrap()
                .snapshot_name()
                .as_deref(),
            Some("cluster-ok")
        );
        for (bad, needle) in [
            ("  playbook: pb/missing.yml\n", "introuvable"),
            ("  playbook: /etc/hosts\n", "relatif"),
            (
                "  playbook: ../../../../../../../etc/hosts\n",
                "sort du répertoire",
            ),
            ("  playbook: pb\n", "n'est pas un fichier"),
            ("  playbook: pb/site.yml\n  snapshot: '-bad'\n", "snapshot"),
        ] {
            let err = format!("{:#}", validate(&lab(bad), &dir).unwrap_err());
            assert!(err.contains(needle), "{bad}: {err}");
        }
        assert!(load_from_str(
            "name: k8s\nansible:\n  playbook: pb/site.yml\n  playbok: x\nmachines:\n  cp:\n    os: debian-12\n"
        )
        .is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subnet_field_drives_ip_validation() {
        let lab = load_from_str(
            "name: k8s\nsubnet: 10.42.0.0/24\nmachines:\n  cp:\n    os: debian-12\n    ip: 10.42.0.10\n",
        )
        .unwrap();
        validate(&lab, Path::new(".")).unwrap();
        assert_eq!(
            crate::lab_network::lab_subnet(&lab).to_string(),
            "10.42.0.0/24"
        );

        let outside = load_from_str(
            "name: k8s\nsubnet: 10.42.0.0/24\nmachines:\n  cp:\n    os: debian-12\n    ip: 10.43.0.10\n",
        )
        .unwrap();
        let err = format!("{:#}", validate(&outside, Path::new(".")).unwrap_err());
        assert!(
            err.contains("hors du réseau du lab (10.42.0.0/24)"),
            "{err}"
        );

        let bad =
            load_from_str("name: k8s\nsubnet: 10.42.0.0/16\nmachines:\n  cp:\n    os: debian-12\n")
                .unwrap();
        assert!(format!("{:#}", validate(&bad, Path::new(".")).unwrap_err()).contains("/24"));

        let mut bridge_only = load_from_str(
            "name: k8s\nnetwork: bridge\nsubnet: 10.42.0.0/24\nmachines:\n  cp:\n    os: debian-12\n",
        )
        .unwrap();
        assert!(validate_subnet(&bridge_only)
            .unwrap_err()
            .to_string()
            .contains("sans effet"));
        bridge_only.subnet = None;
        assert!(validate_subnet(&bridge_only).is_ok());
    }
}
