//! Parse and validate `Virtumfile` (Virtum v0.2 lab-as-code).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::disk::parse_disk_size;
use crate::images::resolve_image;
use crate::metadata::validate_name;

pub const DEFAULT_VIRTUMFILE: &str = "Virtumfile";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Virtumfile {
    pub name: String,
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
    pub ports: Vec<String>,
    #[serde(default)]
    pub provision: Vec<String>,
}

fn default_ram() -> u32 {
    2048
}

fn default_cpu() -> u32 {
    2
}

/// Charge et valide un Virtumfile depuis le disque.
pub fn load(path: &Path) -> Result<Virtumfile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
    load_from_str(&content).with_context(|| format!("Virtumfile invalide: {}", path.display()))
}

pub fn load_from_str(content: &str) -> Result<Virtumfile> {
    let lab: Virtumfile = serde_yaml::from_str(content).context("YAML invalide")?;
    validate(&lab)?;
    Ok(lab)
}

pub fn validate(lab: &Virtumfile) -> Result<()> {
    validate_name(&lab.name).context("Champ `name` (nom du lab) invalide")?;
    if lab.machines.is_empty() {
        bail!("Le Virtumfile doit définir au moins une machine sous `machines`.");
    }
    for (machine_name, spec) in &lab.machines {
        validate_name(machine_name)
            .with_context(|| format!("Nom de machine '{machine_name}' invalide"))?;
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
        for port in &spec.ports {
            parse_port_mapping(port)
                .with_context(|| format!("Machine '{machine_name}': port invalide"))?;
        }
        if !spec.ports.is_empty() {
            bail!(
                "Machine '{machine_name}': le mapping de ports (`ports`) n'est pas encore implémenté \
                 (prévu STEPS2 étape 31). Retirez `ports` pour lancer le lab."
            );
        }
    }
    Ok(())
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

    #[test]
    fn loads_valid_lab() {
        let lab = load_from_str(SAMPLE).unwrap();
        assert_eq!(lab.name, "web-stack-lab");
        assert_eq!(lab.machines.len(), 2);
        assert_eq!(lab.machines["web"].disk_size.as_deref(), Some("10G"));
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
    fn rejects_ports_for_now() {
        let yaml = r#"
name: x
machines:
  web:
    os: debian-12
    ports:
      - "8080:80"
"#;
        assert!(load_from_str(yaml).is_err());
    }

    #[test]
    fn parse_port_mapping_ok() {
        assert_eq!(parse_port_mapping("8080:80").unwrap(), (8080, 80));
    }
}
