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
}
