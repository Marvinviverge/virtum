//! Réseau libvirt isolé par lab (`virtum-lab-<name>`, étape 30).

use std::net::Ipv4Addr;
use std::process::Command;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::command::{path_arg, run_command};
use crate::metadata;

pub const LAB_NETWORK_PREFIX: &str = "virtum-lab-";

/// Nom du réseau libvirt pour un lab Virtumfile (`name:`).
pub fn lab_network_name(lab_name: &str) -> Result<String> {
    metadata::validate_name(lab_name)?;
    let slug = lab_name.trim();
    let full = format!("{LAB_NETWORK_PREFIX}{slug}");
    if full.len() > 64 {
        bail!(
            "Le nom de lab '{lab_name}' est trop long pour le réseau libvirt (max 64 caractères avec le préfixe)."
        );
    }
    Ok(full)
}

/// Pont Linux (max 15 caractères) dérivé du nom du lab.
pub fn lab_bridge_name(lab_name: &str) -> String {
    let digest = Sha256::digest(lab_name.as_bytes());
    format!("vbr{}", hex::encode(&digest[..4]))
}

/// Sous-réseau NAT calculé : 192.168.<octet>.0/24 (évite 122 = réseau `default`).
pub fn lab_subnet_third_octet(lab_name: &str) -> u8 {
    let digest = Sha256::digest(lab_name.as_bytes());
    64 + (digest[1] % 160)
}

/// Sous-réseau /24 d'un lab : `a.b.c.0/24` (`subnet:` déclaré ou calculé depuis le nom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabSubnet {
    pub prefix: [u8; 3],
}

impl LabSubnet {
    /// Calcul par défaut (hash du nom du lab, v0.2).
    pub fn hashed(lab_name: &str) -> Self {
        Self {
            prefix: [192, 168, lab_subnet_third_octet(lab_name)],
        }
    }

    /// `subnet:` du Virtumfile : `a.b.c.0/24`, dans une plage privée, hors `default`.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let (addr, len) = value
            .split_once('/')
            .with_context(|| format!("subnet: '{value}' attendu au format a.b.c.0/24"))?;
        if len != "24" {
            bail!("subnet: '{value}' : seul le préfixe /24 est accepté (ex. 192.168.50.0/24).");
        }
        let ip: Ipv4Addr = addr
            .parse()
            .map_err(|_| anyhow::anyhow!("subnet: '{value}' : adresse IPv4 invalide"))?;
        let [a, b, c, d] = ip.octets();
        if d != 0 {
            bail!("subnet: '{value}' : l'adresse de réseau doit finir par .0 ({a}.{b}.{c}.0/24).");
        }
        if !ip.is_private() {
            bail!(
                "subnet: '{value}' : plage privée attendue (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16)."
            );
        }
        if [a, b, c] == [192, 168, 122] {
            bail!("subnet: '{value}' est le réseau `default` de libvirt.");
        }
        Ok(Self { prefix: [a, b, c] })
    }

    pub fn network(&self) -> Ipv4Addr {
        let [a, b, c] = self.prefix;
        Ipv4Addr::new(a, b, c, 0)
    }

    pub fn host(&self, last: u8) -> Ipv4Addr {
        let [a, b, c] = self.prefix;
        Ipv4Addr::new(a, b, c, last)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        ip.octets()[..3] == self.prefix
    }

    /// Recouvre `net/prefix_len` (n'importe quelle longueur de préfixe).
    pub fn overlaps(&self, net: Ipv4Addr, prefix_len: u8) -> bool {
        let common = prefix_len.min(24);
        let mask = if common == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(common))
        };
        (u32::from(self.network()) & mask) == (u32::from(net) & mask)
    }
}

impl std::fmt::Display for LabSubnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/24", self.network())
    }
}

/// Sous-réseau effectif d'un lab : `subnet:` s'il est déclaré (déjà validé), sinon calculé.
pub fn lab_subnet(lab: &crate::virtumfile::Virtumfile) -> LabSubnet {
    lab.subnet
        .as_deref()
        .and_then(|s| LabSubnet::parse(s).ok())
        .unwrap_or_else(|| LabSubnet::hashed(&lab.name))
}

pub fn generate_lab_network_xml(lab_name: &str, subnet: LabSubnet) -> Result<String> {
    let net_name = lab_network_name(lab_name)?;
    let bridge = lab_bridge_name(lab_name);
    let gateway = subnet.host(1);
    let netmask = "255.255.255.0";
    let range_start = subnet.host(2);
    let range_end = subnet.host(254);

    let owner = crate::ownership::metadata_xml("  ");
    // dnsmasq (v0.9) : `no-ping` — pas de ping de 3 s avant d'attribuer une adresse ;
    // `dhcp-ignore-clid` — baux indexés par la seule MAC : une VM recréée (MAC stable,
    // nouvel identifiant client) retrouve son adresse au lieu d'en prendre une autre.
    Ok(format!(
        r#"<network xmlns:dnsmasq='{DNSMASQ_NS}'>
  <name>{net_name}</name>
{owner}  <forward mode='nat'/>
  <bridge name='{bridge}' stp='on' delay='0'/>
  <domain name='{net_name}'/>
  <ip address='{gateway}' netmask='{netmask}'>
    <dhcp>
      <range start='{range_start}' end='{range_end}'/>
    </dhcp>
  </ip>
  <dnsmasq:options>
    <dnsmasq:option value='no-ping'/>
    <dnsmasq:option value='dhcp-ignore-clid'/>
  </dnsmasq:options>
</network>
"#
    ))
}

/// Espace de noms libvirt des options dnsmasq.
const DNSMASQ_NS: &str = "http://libvirt.org/schemas/network/dnsmasq/1.0";

/// Le réseau défini porte les réglages DHCP de la v0.9 (`no-ping`, `dhcp-ignore-clid`).
pub fn has_dhcp_tuning(network_xml: &str) -> bool {
    network_xml.contains("value='no-ping'") && network_xml.contains("value='dhcp-ignore-clid'")
}

fn defined_network_xml(net_name: &str) -> Result<Option<String>> {
    if !net_is_defined(net_name)? {
        return Ok(None);
    }
    let output = run_virsh(&["net-dumpxml", "--inactive", net_name])?;
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn run_virsh(args: &[&str]) -> Result<std::process::Output> {
    run_command("virsh", args)
}

/// `virsh` sans erreur si le réseau/domaine n'existe pas encore (ex. `net-info` → 1).
fn run_virsh_output(args: &[&str]) -> Result<std::process::Output> {
    Command::new("virsh")
        .args(args)
        .output()
        .with_context(|| format!("Impossible d'exécuter virsh {}", args.join(" ")))
}

fn net_is_defined(net_name: &str) -> Result<bool> {
    let output = run_virsh_output(&["net-info", net_name])?;
    Ok(output.status.success())
}

/// Réseau libvirt du lab déjà défini (`virsh net-info`).
pub fn lab_network_is_defined(net_name: &str) -> Result<bool> {
    net_is_defined(net_name)
}

/// Réseau libvirt du lab actif (`Active: yes`).
pub fn lab_network_is_active(net_name: &str) -> Result<bool> {
    net_is_active(net_name)
}

fn net_is_active(net_name: &str) -> Result<bool> {
    let output = run_virsh_output(&["net-info", net_name])?;
    if !output.status.success() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .find(|l| l.trim_start().starts_with("Active:"))
        .is_some_and(|l| {
            l.split(':')
                .nth(1)
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("yes"))
        }))
}

/// Sous-réseau d'un réseau libvirt déjà défini (`None` s'il n'existe pas).
pub fn defined_subnet(net_name: &str) -> Result<Option<(Ipv4Addr, u8)>> {
    Ok(defined_network_xml(net_name)?.and_then(|xml| parse_network_ip(&xml)))
}

/// Crée ou démarre le réseau isolé du lab ; retourne le nom libvirt (`virtum-lab-…`).
///
/// Si le réseau existe avec un autre sous-réseau (`subnet:` ajouté ou modifié) : recréé
/// quand le lab n'a aucune VM (`can_recreate`), refusé sinon.
pub fn ensure_lab_network(lab_name: &str, subnet: LabSubnet, can_recreate: bool) -> Result<String> {
    let net_name = lab_network_name(lab_name)?;

    if let Some(xml) = defined_network_xml(&net_name)? {
        // Réseau d'un autre Virtum (même nom de lab) : ni réutilisé ni modifié.
        if let crate::ownership::Owner::Other(id) = crate::ownership::network_owner(&net_name) {
            bail!(
                "Le réseau {net_name} appartient à un autre Virtum ({id}) : refus de l'utiliser. \
                 Renommez le lab (`name:`)."
            );
        }
        // Réseau non marqué : réutilisable seulement si aucun domaine d'un autre compte
        // n'y est attaché (sinon nos VM rejoindraient son réseau isolé).
        if matches!(
            crate::ownership::network_owner(&net_name),
            crate::ownership::Owner::Unknown
        ) && crate::ownership::network_has_attached_domains(&net_name)
            && !crate::ownership::network_may_modify(&net_name)
        {
            bail!(
                "Le réseau {net_name} existe sans marqueur Virtum et sert des VM qui ne sont pas \
                 à cet état Virtum : refus de le réutiliser. Renommez le lab (`name:`)."
            );
        }
        let current = parse_network_ip(&xml);
        let subnet_differs = !current.is_some_and(|(addr, len)| len == 24 && subnet.contains(addr));
        if subnet_differs && !can_recreate {
            let (addr, len) = current.unwrap_or((Ipv4Addr::UNSPECIFIED, 0));
            bail!(
                "Le réseau {net_name} existe en {addr}/{len}, le Virtumfile demande {subnet}. \
                 Les VM du lab en dépendent : `virtum down -f … --destroy` puis `virtum up` pour changer de sous-réseau."
            );
        }
        // Sans VM, le réseau est recréé (sous-réseau changé ou réglages DHCP d'avant la
        // v0.9) ; avec des VM, un réseau sans réglages DHCP reste tel quel.
        // Réseau partagé sur qemu:///system : ne recréer qu'un réseau marqué par ce Virtum,
        // ou un réseau non marqué (créé avant le marqueur) qu'aucune VM n'utilise.
        let ours = match crate::ownership::network_owner(&net_name) {
            crate::ownership::Owner::Us => true,
            crate::ownership::Owner::Other(_) => false,
            crate::ownership::Owner::Unknown => {
                !crate::ownership::network_has_attached_domains(&net_name)
            }
        };
        if can_recreate && !ours && subnet_differs {
            bail!(
                "Le réseau {net_name} existe déjà (autre utilisateur ou VM attachées) avec un autre \
                 sous-réseau : refus de le recréer. Renommez le lab (`name:`) ou supprimez ce \
                 réseau explicitement."
            );
        }
        if can_recreate && ours && (subnet_differs || !has_dhcp_tuning(&xml)) {
            let why = if subnet_differs {
                "sous-réseau changé"
            } else {
                "réglages DHCP v0.9"
            };
            crate::verr!("Réseau {net_name} recréé en {subnet} ({why}, aucune VM).");
            let _ = run_virsh(&["net-destroy", &net_name]);
            run_virsh(&["net-undefine", &net_name])
                .with_context(|| format!("virsh net-undefine a échoué pour '{net_name}'"))?;
        }
    }

    if net_is_active(&net_name)? {
        return Ok(net_name);
    }

    if !net_is_defined(&net_name)? {
        let xml = generate_lab_network_xml(lab_name, subnet)?;
        // Fichier temporaire au nom imprévisible, créé en exclusif (pas de lien suivi).
        let tmp = crate::fsutil::write_new_temp(
            &std::env::temp_dir(),
            &format!("virtum-{net_name}"),
            xml.as_bytes(),
        )?;
        run_virsh(&["net-define", path_arg(&tmp)?]).with_context(|| {
            format!(
                "virsh net-define a échoué pour le réseau '{net_name}'. \
                 Vérifiez les droits libvirt (virtum doctor)."
            )
        })?;
        let _ = std::fs::remove_file(&tmp);
        run_virsh(&["net-autostart", &net_name]).context("virsh net-autostart a échoué")?;
    }

    run_virsh(&["net-start", &net_name]).with_context(|| {
        format!(
            "virsh net-start a échoué pour '{net_name}'. \
             Essayez: sudo virsh net-start {net_name}"
        )
    })?;

    if !net_is_active(&net_name)? {
        bail!("Le réseau '{net_name}' n'est pas actif après net-start.");
    }

    crate::verr!(
        "Réseau lab actif: {net_name} (NAT {subnet}, pont {})",
        lab_bridge_name(lab_name)
    );
    Ok(net_name)
}

// --- Chevauchements de sous-réseaux (v0.9) --------------------------------------------

/// Adresse + longueur de préfixe de la première `<ip>` IPv4 d'un XML de réseau libvirt
/// (`netmask='255.255.255.0'` ou `prefix='24'`).
pub fn parse_network_ip(xml: &str) -> Option<(Ipv4Addr, u8)> {
    xml.lines().map(str::trim).find_map(|line| {
        if !line.starts_with("<ip ") || line.contains("family='ipv6'") {
            return None;
        }
        let addr: Ipv4Addr = xml_attr(line, "address")?.parse().ok()?;
        let len = match (xml_attr(line, "prefix"), xml_attr(line, "netmask")) {
            (Some(p), _) => p.parse().ok()?,
            (None, Some(mask)) => u32::from(mask.parse::<Ipv4Addr>().ok()?).count_ones() as u8,
            (None, None) => 24,
        };
        Some((addr, len))
    })
}

/// Pont Linux d'un XML de réseau libvirt (`<bridge name='…'/>`).
fn parse_network_bridge(xml: &str) -> Option<String> {
    xml.lines()
        .map(str::trim)
        .find(|l| l.starts_with("<bridge "))
        .and_then(|l| xml_attr(l, "name"))
}

/// Adresses IPv4 des interfaces de l'hôte (`ip -o -4 addr show`) : `(interface, adresse, préfixe)`.
pub fn parse_ip_addr(output: &str) -> Vec<(String, Ipv4Addr, u8)> {
    output
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let iface = tokens.nth(1)?.trim_end_matches(':').to_string();
            let cidr = tokens.skip_while(|t| *t != "inet").nth(1)?;
            let (addr, len) = cidr.split_once('/')?;
            Some((iface, addr.parse().ok()?, len.parse().ok()?))
        })
        .collect()
}

/// Réseaux déjà utilisés sur l'hôte : `(description, réseau, préfixe)`, hors réseau du lab.
pub fn used_networks(own_net: &str, own_bridge: &str) -> Vec<(String, Ipv4Addr, u8)> {
    let mut used = Vec::new();
    let mut libvirt_bridges = vec![own_bridge.to_string()];
    let names = run_virsh_output(&["net-list", "--all", "--name"])
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for net in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
        let Ok(out) = run_virsh_output(&["net-dumpxml", "--inactive", net]) else {
            continue;
        };
        let xml = String::from_utf8_lossy(&out.stdout);
        if let Some(bridge) = parse_network_bridge(&xml) {
            libvirt_bridges.push(bridge);
        }
        if net == own_net {
            continue;
        }
        if let Some((addr, len)) = parse_network_ip(&xml) {
            used.push((format!("réseau libvirt {net}"), addr, len));
        }
    }
    let addrs = Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for (iface, addr, len) in parse_ip_addr(&addrs) {
        if addr.is_loopback() || libvirt_bridges.contains(&iface) {
            continue;
        }
        used.push((format!("interface {iface}"), addr, len));
    }
    used
}

/// Premier 192.168.X.0/24 (X dans la plage du calcul par défaut) qui ne recouvre rien.
pub fn suggest_free_subnet(used: &[(String, Ipv4Addr, u8)]) -> Option<LabSubnet> {
    (64u8..=223)
        .map(|x| LabSubnet {
            prefix: [192, 168, x],
        })
        .find(|s| !used.iter().any(|(_, net, len)| s.overlaps(*net, *len)))
}

/// Chevauchements du sous-réseau du lab avec les autres réseaux de l'hôte (message prêt
/// à afficher, avec un sous-réseau libre proposé), vide si aucun.
pub fn subnet_conflicts(lab: &crate::virtumfile::Virtumfile) -> Result<Vec<String>> {
    if !crate::virtumfile::lab_needs_isolated_network(lab) {
        return Ok(Vec::new());
    }
    let subnet = lab_subnet(lab);
    let net_name = lab_network_name(&lab.name)?;
    let used = used_networks(&net_name, &lab_bridge_name(&lab.name));
    Ok(conflict_messages(lab, subnet, &used))
}

fn conflict_messages(
    lab: &crate::virtumfile::Virtumfile,
    subnet: LabSubnet,
    used: &[(String, Ipv4Addr, u8)],
) -> Vec<String> {
    let clashes: Vec<String> = used
        .iter()
        .filter(|(_, net, len)| subnet.overlaps(*net, *len))
        .map(|(what, net, len)| format!("{what} ({net}/{len})"))
        .collect();
    if clashes.is_empty() {
        return Vec::new();
    }
    let origin = if lab.subnet.is_some() {
        "déclaré"
    } else {
        "calculé depuis le nom du lab"
    };
    let hint = suggest_free_subnet(used)
        .map(|s| format!(" Déclarez un autre sous-réseau, ex. `subnet: {s}`."))
        .unwrap_or_else(|| " Déclarez un autre sous-réseau avec `subnet:`.".to_string());
    vec![format!(
        "Le sous-réseau du lab '{}' ({subnet}, {origin}) est déjà utilisé : {}.{hint}",
        lab.name,
        clashes.join(", ")
    )]
}

/// Détruit le réseau libvirt du lab (idempotent, après suppression des VM).
pub fn destroy_lab_network(lab_name: &str) -> Result<()> {
    let net_name = lab_network_name(lab_name)?;
    if !net_is_defined(&net_name)? {
        return Ok(());
    }
    if !crate::ownership::network_may_destroy(&net_name) {
        crate::verr!(
            "Réseau {net_name} conservé : il n'est pas marqué par cet état Virtum ou des VM y sont \
             encore attachées."
        );
        return Ok(());
    }
    let _ = run_virsh(&["net-destroy", &net_name]);
    run_virsh(&["net-undefine", &net_name]).ok();
    crate::verr!("Réseau lab supprimé: {net_name}");
    Ok(())
}

/// Réseau libvirt à utiliser pour une instance (lab ou `default`).
pub fn network_for_lab(lab_name: Option<&str>) -> Result<String> {
    match lab_name {
        Some(lab) => Ok(lab_network_name(lab)?),
        None => Ok(crate::libvirt::LIBVIRT_DEFAULT_NETWORK.to_string()),
    }
}

// --- Adresses déclarées (`ip:`, v0.7) ------------------------------------------------

/// Réservation DHCP `<host mac=… name=… ip=…/>` du réseau du lab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpHost {
    pub mac: String,
    pub name: Option<String>,
    pub ip: String,
}

impl DhcpHost {
    fn to_xml(&self) -> String {
        use crate::libvirt::xml_escape;
        let name = self
            .name
            .as_deref()
            .map(|n| format!(" name='{}'", xml_escape(n)))
            .unwrap_or_default();
        format!(
            "<host mac='{}'{name} ip='{}'/>",
            xml_escape(&self.mac),
            xml_escape(&self.ip)
        )
    }
}

/// Vérifie qu'une adresse déclarée est utilisable sur le réseau du lab.
pub fn validate_lab_ip(subnet: LabSubnet, ip: Ipv4Addr) -> Result<()> {
    if !subnet.contains(ip) {
        bail!("{ip} est hors du réseau du lab ({subnet}).");
    }
    match ip.octets()[3] {
        0 => bail!("{ip} est l'adresse du réseau."),
        1 => bail!("{ip} est réservée (passerelle du lab)."),
        255 => bail!("{ip} est l'adresse de diffusion."),
        _ => Ok(()),
    }
}

/// MAC stable pour une machine de lab : `52:54:00` (préfixe QEMU) + 3 octets de
/// `sha256(vm_id)`. En cas de collision avec `taken`, essaie `vm_id-1`, `vm_id-2`…
pub fn deterministic_mac(vm_id: &str, taken: &[String]) -> String {
    let mut attempt = 0u32;
    loop {
        let seed = if attempt == 0 {
            vm_id.to_string()
        } else {
            format!("{vm_id}-{attempt}")
        };
        let digest = Sha256::digest(seed.as_bytes());
        let mac = format!(
            "52:54:00:{:02x}:{:02x}:{:02x}",
            digest[0], digest[1], digest[2]
        );
        if !taken.iter().any(|t| t.eq_ignore_ascii_case(&mac)) {
            return mac;
        }
        attempt += 1;
    }
}

/// Réservations `<host …/>` présentes dans un XML de réseau libvirt.
pub fn parse_dhcp_hosts(network_xml: &str) -> Vec<DhcpHost> {
    network_xml
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("<host "))
        .filter_map(|l| {
            Some(DhcpHost {
                mac: xml_attr(l, "mac")?.to_lowercase(),
                name: xml_attr(l, "name"),
                ip: xml_attr(l, "ip")?,
            })
        })
        .collect()
}

fn xml_attr(line: &str, attr: &str) -> Option<String> {
    let rest = line.split(&format!(" {attr}=")).nth(1)?;
    let quote = rest.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let value = &rest[1..];
    Some(value[..value.find(quote)?].to_string())
}

/// Bail DHCP lu dans `virsh net-dhcp-leases`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpLease {
    pub mac: String,
    pub ip: String,
    /// Expiration telle qu'affichée par virsh (`2026-11-12 23:43:35`, heure locale).
    pub expiry: Option<String>,
}

pub fn parse_dhcp_leases(output: &str) -> Vec<DhcpLease> {
    output
        .lines()
        .filter_map(crate::libvirt::parse_dhcp_lease_line)
        .map(|l| DhcpLease {
            mac: l.mac,
            ip: l.ip,
            expiry: Some(l.expiry),
        })
        .collect()
}

/// Réservations actuelles du réseau (définition persistante).
pub fn dhcp_hosts(net_name: &str) -> Result<Vec<DhcpHost>> {
    if !net_is_defined(net_name)? {
        return Ok(Vec::new());
    }
    let output = run_virsh(&["net-dumpxml", "--inactive", net_name])?;
    Ok(parse_dhcp_hosts(&String::from_utf8_lossy(&output.stdout)))
}

/// Baux en cours sur le réseau (vide si le réseau est absent ou inactif).
pub fn dhcp_leases(net_name: &str) -> Vec<DhcpLease> {
    match crate::libvirt::net_dhcp_leases(net_name) {
        Ok(out) => parse_dhcp_leases(&out),
        Err(_) => Vec::new(),
    }
}

fn net_update(net_name: &str, command: &str, host: &DhcpHost) -> Result<()> {
    if !crate::ownership::network_may_modify(net_name) {
        bail!(
            "Le réseau {net_name} n'appartient pas à cet état Virtum : réservation DHCP non modifiée."
        );
    }
    let xml = host.to_xml();
    let mut args = vec![
        "net-update",
        net_name,
        command,
        "ip-dhcp-host",
        xml.as_str(),
    ];
    if net_is_active(net_name)? {
        args.push("--live");
    }
    args.push("--config");
    run_virsh(&args)
        .map(|_| ())
        .with_context(|| format!("virsh net-update {command} a échoué sur '{net_name}' ({xml})"))
}

/// Réserve `ip` pour `mac` (idempotent). Retire d'abord toute réservation de cette MAC
/// ou de cette adresse (ancienne IP, VM détruite) pour éviter les doublons dnsmasq.
pub fn ensure_dhcp_reservation(net_name: &str, mac: &str, name: &str, ip: &str) -> Result<()> {
    let wanted = DhcpHost {
        mac: mac.to_lowercase(),
        name: Some(name.to_string()),
        ip: ip.to_string(),
    };
    let hosts = dhcp_hosts(net_name)?;
    if hosts.contains(&wanted) {
        return Ok(());
    }
    for host in hosts
        .iter()
        .filter(|h| h.mac == wanted.mac || h.ip == wanted.ip)
    {
        net_update(net_name, "delete", host)?;
    }
    net_update(net_name, "add-last", &wanted)
}

/// Supprime les réservations d'une MAC (retrait de `ip:`, `virtum destroy`). Idempotent.
pub fn remove_dhcp_reservations(net_name: &str, mac: &str) -> Result<usize> {
    let mac = mac.to_lowercase();
    let hosts = dhcp_hosts(net_name)?;
    let mut removed = 0;
    for host in hosts.iter().filter(|h| h.mac == mac) {
        net_update(net_name, "delete", host)?;
        removed += 1;
    }
    Ok(removed)
}

/// MAC de la carte lab (première carte) de chaque machine déjà déployée : `(machine, mac)`.
pub fn lab_instance_macs(
    instances_dir: &std::path::Path,
    instances: &[&metadata::Instance],
) -> Vec<(String, String)> {
    instances
        .iter()
        .filter_map(|inst| {
            let machine = inst.machine_name.clone()?;
            let mac = crate::libvirt::resolve_nic_mac(&instances_dir.join(&inst.id), &inst.id)?;
            Some((machine, mac.to_lowercase()))
        })
        .collect()
}

/// Adresse déclarée par une machine : `(machine, ip, mac de sa carte lab si connue)`.
pub struct DeclaredIp<'a> {
    pub machine: &'a str,
    pub ip: String,
    pub mac: Option<String>,
}

/// Conflits bloquants : l'adresse est louée ou réservée pour la carte d'une **autre**
/// machine du lab encore déployée. Un bail ou une réservation d'une MAC inconnue
/// (VM détruite) n'est pas bloquant : la réservation est remplacée au `up` / `apply`.
pub fn ip_conflicts(
    declared: &[DeclaredIp<'_>],
    owners: &[(String, String)],
    leases: &[DhcpLease],
    hosts: &[DhcpHost],
) -> Vec<String> {
    let mut errors = Vec::new();
    for d in declared {
        let holders = leases
            .iter()
            .map(|l| (l.mac.as_str(), l.ip.as_str(), "louée"))
            .chain(
                hosts
                    .iter()
                    .map(|h| (h.mac.as_str(), h.ip.as_str(), "réservée")),
            );
        for (mac, ip, how) in holders {
            if ip != d.ip
                || d.mac
                    .as_deref()
                    .is_some_and(|own| own.eq_ignore_ascii_case(mac))
            {
                continue;
            }
            if let Some((other, _)) = owners
                .iter()
                .find(|(m, owner_mac)| m != d.machine && owner_mac.eq_ignore_ascii_case(mac))
            {
                let msg = format!(
                    "Machine '{}' : {} est déjà {how} pour la machine '{other}'. \
                     Choisissez une autre adresse ou détruisez '{other}'.",
                    d.machine, d.ip
                );
                errors.push(msg);
                break; // un seul message par adresse (bail et réservation vont ensemble)
            }
        }
    }
    errors
}

/// Avertissements : l'adresse déclarée est encore louée à une MAC qu'aucune machine
/// du lab n'utilise (VM détruite). dnsmasq ne la donne qu'à l'expiration de ce bail ;
/// d'ici là, la machine reçoit une autre adresse (vérifié sur un lab réel).
pub fn stale_lease_warnings(
    declared: &[DeclaredIp<'_>],
    owners: &[(String, String)],
    leases: &[DhcpLease],
) -> Vec<String> {
    declared
        .iter()
        .filter_map(|d| {
            let lease = leases.iter().find(|l| {
                l.ip == d.ip
                    && !d
                        .mac
                        .as_deref()
                        .is_some_and(|own| own.eq_ignore_ascii_case(&l.mac))
                    && !owners
                        .iter()
                        .any(|(_, mac)| mac.eq_ignore_ascii_case(&l.mac))
            })?;
            let until = lease
                .expiry
                .as_deref()
                .map(|e| format!(" jusqu'à {e}"))
                .unwrap_or_default();
            Some(format!(
                "Machine '{m}' : {ip} est encore louée à une VM supprimée ({mac}){until}. \
                 D'ici là, la machine recevra une autre adresse ; ensuite \
                 `virtum stop {m}` / `virtum start {m}` la ramène sur {ip}.",
                m = d.machine,
                ip = d.ip,
                mac = lease.mac,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_network_name_from_demo_lab() {
        assert_eq!(lab_network_name("demo-lab").unwrap(), "virtum-lab-demo-lab");
    }

    #[test]
    fn bridge_name_length() {
        let b = lab_bridge_name("demo-lab");
        assert!(b.len() <= 15);
        assert!(b.starts_with("vbr"));
    }

    #[test]
    fn subnet_octet_in_private_range() {
        let o = lab_subnet_third_octet("demo-lab");
        assert!((64..=223).contains(&o));
        assert_ne!(o, 122);
    }

    #[test]
    fn network_xml_contains_name_and_bridge() {
        let xml = generate_lab_network_xml("demo-lab", LabSubnet::hashed("demo-lab")).unwrap();
        assert!(xml.contains("<name>virtum-lab-demo-lab</name>"));
        assert!(xml.contains("bridge name='vbr"));
        assert!(xml.contains("192.168."));
        let declared = LabSubnet::parse("10.20.30.0/24").unwrap();
        let xml = generate_lab_network_xml("demo-lab", declared).unwrap();
        assert!(xml.contains("<ip address='10.20.30.1' netmask='255.255.255.0'>"));
        assert!(xml.contains("<range start='10.20.30.2' end='10.20.30.254'/>"));
        assert!(xml.starts_with(
            "<network xmlns:dnsmasq='http://libvirt.org/schemas/network/dnsmasq/1.0'>"
        ));
        assert!(has_dhcp_tuning(&xml));
        assert!(!has_dhcp_tuning("<network><name>old</name></network>"));
        assert_eq!(
            parse_network_ip(&xml),
            Some((Ipv4Addr::new(10, 20, 30, 1), 24))
        );
    }

    #[test]
    fn subnet_parsing_rules() {
        assert_eq!(
            LabSubnet::parse("192.168.50.0/24").unwrap().to_string(),
            "192.168.50.0/24"
        );
        assert!(LabSubnet::parse("172.20.1.0/24").is_ok());
        for (bad, needle) in [
            ("192.168.50.0", "format"),
            ("192.168.50.0/16", "/24"),
            ("192.168.50.7/24", ".0"),
            ("8.8.8.0/24", "privée"),
            ("192.168.122.0/24", "default"),
            ("192.168.300.0/24", "invalide"),
        ] {
            let err = LabSubnet::parse(bad).unwrap_err().to_string();
            assert!(err.contains(needle), "{bad}: {err}");
        }
    }

    #[test]
    fn overlaps_and_free_subnet() {
        let s = LabSubnet::parse("192.168.68.0/24").unwrap();
        assert!(s.overlaps(Ipv4Addr::new(192, 168, 68, 1), 24));
        assert!(s.overlaps(Ipv4Addr::new(192, 168, 0, 0), 16));
        assert!(s.overlaps(Ipv4Addr::new(192, 168, 68, 128), 25));
        assert!(!s.overlaps(Ipv4Addr::new(192, 168, 69, 1), 24));
        assert!(!s.overlaps(Ipv4Addr::new(10, 0, 0, 1), 8));
        let used = vec![
            ("a".to_string(), Ipv4Addr::new(192, 168, 64, 1), 24),
            ("b".to_string(), Ipv4Addr::new(192, 168, 65, 1), 24),
        ];
        assert_eq!(
            suggest_free_subnet(&used).unwrap().to_string(),
            "192.168.66.0/24"
        );
    }

    #[test]
    fn conflict_message_names_the_culprit_and_a_free_subnet() {
        let lab = crate::virtumfile::load_from_str(
            "name: benchfail\nsubnet: 192.168.68.0/24\nmachines:\n  a:\n    os: debian-12\n",
        )
        .unwrap();
        let used = vec![(
            "réseau libvirt virtum-lab-demo-lab".to_string(),
            Ipv4Addr::new(192, 168, 68, 1),
            24,
        )];
        let msgs = conflict_messages(&lab, lab_subnet(&lab), &used);
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].contains("virtum-lab-demo-lab (192.168.68.1/24)"),
            "{}",
            msgs[0]
        );
        assert!(msgs[0].contains("déclaré"), "{}", msgs[0]);
        assert!(msgs[0].contains("`subnet: 192.168.64.0/24`"), "{}", msgs[0]);
        assert!(conflict_messages(&lab, lab_subnet(&lab), &[]).is_empty());
    }

    #[test]
    fn parses_host_addresses() {
        let out = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever\n\
                   3: wlp3s0    inet 192.168.1.23/24 brd 192.168.1.255 scope global dynamic wlp3s0\n";
        assert_eq!(
            parse_ip_addr(out),
            vec![
                ("lo".to_string(), Ipv4Addr::new(127, 0, 0, 1), 8),
                ("wlp3s0".to_string(), Ipv4Addr::new(192, 168, 1, 23), 24),
            ]
        );
        let xml = "<network>\n  <bridge name='virbr0' stp='on'/>\n  <ip address='192.168.122.1' prefix='24'>\n</network>";
        assert_eq!(
            parse_network_ip(xml),
            Some((Ipv4Addr::new(192, 168, 122, 1), 24))
        );
        assert_eq!(parse_network_bridge(xml).as_deref(), Some("virbr0"));
    }

    #[test]
    fn validate_lab_ip_rules() {
        let octet = lab_subnet_third_octet("k8s");
        let subnet = LabSubnet::hashed("k8s");
        let ip = |d: u8| std::net::Ipv4Addr::new(192, 168, octet, d);
        assert!(validate_lab_ip(subnet, ip(10)).is_ok());
        assert!(validate_lab_ip(subnet, ip(254)).is_ok());
        for bad in [0, 1, 255] {
            assert!(validate_lab_ip(subnet, ip(bad)).is_err(), "{bad}");
        }
        let other = if octet == 200 { 201 } else { 200 };
        let err = validate_lab_ip(subnet, std::net::Ipv4Addr::new(192, 168, other, 10))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&subnet.to_string()), "{err}");
        assert!(validate_lab_ip(subnet, std::net::Ipv4Addr::new(10, 0, 0, 5)).is_err());
        let declared = LabSubnet::parse("10.5.5.0/24").unwrap();
        assert!(validate_lab_ip(declared, std::net::Ipv4Addr::new(10, 5, 5, 7)).is_ok());
    }

    #[test]
    fn deterministic_mac_is_stable_and_avoids_collisions() {
        let a = deterministic_mac("vm-0123456789ab", &[]);
        assert_eq!(a, deterministic_mac("vm-0123456789ab", &[]));
        assert!(a.starts_with("52:54:00:"));
        assert_eq!(a.len(), 17);
        let b = deterministic_mac("vm-0123456789ab", &[a.to_uppercase()]);
        assert_ne!(a, b);
        assert_ne!(a, deterministic_mac("vm-ba9876543210", &[]));
    }

    #[test]
    fn parses_dhcp_hosts_from_network_xml() {
        let xml = r#"<network>
  <name>virtum-lab-k8s</name>
  <ip address='192.168.218.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='192.168.218.2' end='192.168.218.254'/>
      <host mac='52:54:00:AA:BB:CC' name='k8s-control-plane' ip='192.168.218.10'/>
      <host mac="52:54:00:11:22:33" ip="192.168.218.11"/>
    </dhcp>
  </ip>
</network>"#;
        assert_eq!(
            parse_dhcp_hosts(xml),
            vec![
                DhcpHost {
                    mac: "52:54:00:aa:bb:cc".into(),
                    name: Some("k8s-control-plane".into()),
                    ip: "192.168.218.10".into(),
                },
                DhcpHost {
                    mac: "52:54:00:11:22:33".into(),
                    name: None,
                    ip: "192.168.218.11".into(),
                },
            ]
        );
        assert_eq!(
            parse_dhcp_hosts(xml)[0].to_xml(),
            "<host mac='52:54:00:aa:bb:cc' name='k8s-control-plane' ip='192.168.218.10'/>"
        );
    }

    #[test]
    fn parses_dhcp_leases() {
        let out = r#" Expiry Time           MAC address         Protocol   IP address          Hostname   Client ID or DUID
-----------------------------------------------------------------------------------------------------------
 2026-11-12 18:01:02   52:54:00:aa:bb:cc   ipv4       192.168.218.10/24   k8s-cp     ff:00
 2026-11-12 18:03:00   52:54:00:11:22:33   ipv4       192.168.218.57/24   -          -
"#;
        assert_eq!(
            parse_dhcp_leases(out),
            vec![
                DhcpLease {
                    mac: "52:54:00:aa:bb:cc".into(),
                    ip: "192.168.218.10".into(),
                    expiry: Some("2026-11-12 18:01:02".into()),
                },
                DhcpLease {
                    mac: "52:54:00:11:22:33".into(),
                    ip: "192.168.218.57".into(),
                    expiry: Some("2026-11-12 18:03:00".into()),
                },
            ]
        );
    }

    #[test]
    fn dhcp_lease_ignores_guest_chosen_client_id_and_hostname() {
        // L'invité A (52:54:00:aa:aa:aa) annonce la MAC de B comme client-id et nom d'hôte.
        let out = r#" Expiry Time           MAC address         Protocol   IP address          Hostname            Client ID or DUID
---------------------------------------------------------------------------------------------------------------------
 2026-11-12 18:01:02   52:54:00:aa:aa:aa   ipv4       192.168.156.10/24   52:54:00:bb:bb:bb   52:54:00:bb:bb:bb
"#;
        let leases = parse_dhcp_leases(out);
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].mac, "52:54:00:aa:aa:aa");
        assert_eq!(
            crate::libvirt::parse_ipv4_from_dhcp_leases(out, "52:54:00:bb:bb:bb"),
            None
        );
        assert_eq!(
            crate::libvirt::parse_ipv4_from_dhcp_leases(out, "52:54:00:AA:AA:AA").as_deref(),
            Some("192.168.156.10")
        );
        // Hostname absent (« - ») et IP invalide.
        assert!(parse_dhcp_leases(
            " 2026-11-12 18:01:02   52:54:00:aa:aa:aa   ipv4   999.1.1.1/24   -   -"
        )
        .is_empty());
    }

    #[test]
    fn ip_conflicts_only_block_other_live_machines() {
        let owners = vec![
            ("cp".to_string(), "52:54:00:00:00:01".to_string()),
            ("w1".to_string(), "52:54:00:00:00:02".to_string()),
        ];
        let lease = |mac: &str, ip: &str| DhcpLease {
            mac: mac.into(),
            ip: ip.into(),
            expiry: Some("2026-11-12 23:43:35".into()),
        };
        let leases = vec![
            lease("52:54:00:00:00:02", "192.168.218.10"),
            lease("52:54:00:99:99:99", "192.168.218.20"),
        ];
        let declared = |machine, ip: &str, mac: Option<&str>| DeclaredIp {
            machine,
            ip: ip.to_string(),
            mac: mac.map(str::to_string),
        };
        // .10 louée à w1 (vivante) : bloquant pour cp.
        let errs = ip_conflicts(
            &[declared("cp", "192.168.218.10", Some("52:54:00:00:00:01"))],
            &owners,
            &leases,
            &[],
        );
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("'w1'"), "{}", errs[0]);
        // w1 garde sa propre adresse : pas de conflit.
        assert!(ip_conflicts(
            &[declared("w1", "192.168.218.10", Some("52:54:00:00:00:02"))],
            &owners,
            &leases,
            &[],
        )
        .is_empty());
        // .20 louée à une MAC inconnue (VM détruite) : non bloquant, mais averti.
        assert!(ip_conflicts(
            &[declared("cp", "192.168.218.20", None)],
            &owners,
            &leases,
            &[]
        )
        .is_empty());
        let warns =
            stale_lease_warnings(&[declared("cp", "192.168.218.20", None)], &owners, &leases);
        assert_eq!(warns.len(), 1);
        assert!(
            warns[0].contains("jusqu'à 2026-11-12 23:43:35"),
            "{}",
            warns[0]
        );
        // Bail de la machine elle-même ou d'une autre machine vivante : pas d'avertissement.
        assert!(stale_lease_warnings(
            &[declared("w1", "192.168.218.10", Some("52:54:00:00:00:02"))],
            &owners,
            &leases,
        )
        .is_empty());
        assert!(stale_lease_warnings(
            &[declared("cp", "192.168.218.10", Some("52:54:00:00:00:01"))],
            &owners,
            &leases,
        )
        .is_empty());
        // Réservation d'une autre machine vivante : bloquant.
        let hosts = vec![DhcpHost {
            mac: "52:54:00:00:00:01".into(),
            name: None,
            ip: "192.168.218.30".into(),
        }];
        assert_eq!(
            ip_conflicts(
                &[declared("w1", "192.168.218.30", None)],
                &owners,
                &[],
                &hosts
            )
            .len(),
            1
        );
    }
}
