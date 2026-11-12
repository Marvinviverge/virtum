//! Réseau libvirt isolé par lab (`virtum-lab-<name>`, étape 30).

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

/// Sous-réseau NAT dédié : 192.168.<octet>.0/24 (évite 122 = réseau `default`).
pub fn lab_subnet_third_octet(lab_name: &str) -> u8 {
    let digest = Sha256::digest(lab_name.as_bytes());
    64 + (digest[1] % 160)
}

pub fn lab_gateway_ipv4(lab_name: &str) -> String {
    format!("192.168.{}.1", lab_subnet_third_octet(lab_name))
}

pub fn generate_lab_network_xml(lab_name: &str) -> Result<String> {
    let net_name = lab_network_name(lab_name)?;
    let bridge = lab_bridge_name(lab_name);
    let octet = lab_subnet_third_octet(lab_name);
    let gateway = format!("192.168.{octet}.1");
    let netmask = "255.255.255.0";
    let range_start = format!("192.168.{octet}.2");
    let range_end = format!("192.168.{octet}.254");

    Ok(format!(
        r#"<network>
  <name>{net_name}</name>
  <forward mode='nat'/>
  <bridge name='{bridge}' stp='on' delay='0'/>
  <domain name='{net_name}'/>
  <ip address='{gateway}' netmask='{netmask}'>
    <dhcp>
      <range start='{range_start}' end='{range_end}'/>
    </dhcp>
  </ip>
</network>
"#
    ))
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

/// Crée ou démarre le réseau isolé du lab ; retourne le nom libvirt (`virtum-lab-…`).
pub fn ensure_lab_network(lab_name: &str) -> Result<String> {
    let net_name = lab_network_name(lab_name)?;

    if net_is_active(&net_name)? {
        return Ok(net_name);
    }

    if !net_is_defined(&net_name)? {
        let xml = generate_lab_network_xml(lab_name)?;
        let tmp = std::env::temp_dir().join(format!("{net_name}.xml"));
        std::fs::write(&tmp, &xml)
            .with_context(|| format!("Impossible d'écrire {}", tmp.display()))?;
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

    eprintln!(
        "Réseau lab actif: {net_name} (NAT 192.168.{}.0/24, pont {})",
        lab_subnet_third_octet(lab_name),
        lab_bridge_name(lab_name)
    );
    Ok(net_name)
}

/// Détruit le réseau libvirt du lab (idempotent, après suppression des VM).
pub fn destroy_lab_network(lab_name: &str) -> Result<()> {
    let net_name = lab_network_name(lab_name)?;
    if !net_is_defined(&net_name)? {
        return Ok(());
    }
    let _ = run_virsh(&["net-destroy", &net_name]);
    run_virsh(&["net-undefine", &net_name]).ok();
    eprintln!("Réseau lab supprimé: {net_name}");
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
        let name = self
            .name
            .as_deref()
            .map(|n| format!(" name='{n}'"))
            .unwrap_or_default();
        format!("<host mac='{}'{name} ip='{}'/>", self.mac, self.ip)
    }
}

/// Sous-réseau du lab au format `192.168.<octet>.0/24`.
pub fn lab_subnet_label(lab_name: &str) -> String {
    format!("192.168.{}.0/24", lab_subnet_third_octet(lab_name))
}

/// Vérifie qu'une adresse déclarée est utilisable sur le réseau du lab.
pub fn validate_lab_ip(lab_name: &str, ip: std::net::Ipv4Addr) -> Result<()> {
    let [a, b, c, d] = ip.octets();
    let octet = lab_subnet_third_octet(lab_name);
    if (a, b, c) != (192, 168, octet) {
        bail!(
            "{ip} est hors du réseau du lab ({}).",
            lab_subnet_label(lab_name)
        );
    }
    match d {
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
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let mut mac = None;
            let mut ip = None;
            for token in &tokens {
                if token.len() == 17 && token.matches(':').count() == 5 {
                    mac = Some(token.to_lowercase());
                } else if token.contains('/') && token.matches('.').count() == 3 {
                    ip = token.split('/').next().map(str::to_string);
                }
            }
            let expiry = match tokens.as_slice() {
                [date, time, ..] if date.matches('-').count() == 2 && time.contains(':') => {
                    Some(format!("{date} {time}"))
                }
                _ => None,
            };
            Some(DhcpLease {
                mac: mac?,
                ip: ip?,
                expiry,
            })
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
        let xml = generate_lab_network_xml("demo-lab").unwrap();
        assert!(xml.contains("<name>virtum-lab-demo-lab</name>"));
        assert!(xml.contains("bridge name='vbr"));
        assert!(xml.contains("192.168."));
    }

    #[test]
    fn validate_lab_ip_rules() {
        let octet = lab_subnet_third_octet("k8s");
        let ip = |d: u8| std::net::Ipv4Addr::new(192, 168, octet, d);
        assert!(validate_lab_ip("k8s", ip(10)).is_ok());
        assert!(validate_lab_ip("k8s", ip(254)).is_ok());
        for bad in [0, 1, 255] {
            assert!(validate_lab_ip("k8s", ip(bad)).is_err(), "{bad}");
        }
        let other = if octet == 200 { 201 } else { 200 };
        let err = validate_lab_ip("k8s", std::net::Ipv4Addr::new(192, 168, other, 10))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&lab_subnet_label("k8s")), "{err}");
        assert!(validate_lab_ip("k8s", std::net::Ipv4Addr::new(10, 0, 0, 5)).is_err());
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
