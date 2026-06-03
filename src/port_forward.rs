//! Port mapping hôte → invité (STEPS2 étape 31, v0.3 `bind: lan`) via `socat`.
//!
//! Les proxies tournent en processus séparé (survit à la fin de `virtum up`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bridge_network;
use crate::metadata::{Instance, InstanceStatus, Metadata};

const FORWARD_PID_FILE: &str = "port-forwards.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PortBind {
    /// `127.0.0.1` uniquement (défaut v0.2).
    #[default]
    Local,
    /// Toutes les interfaces — joignable depuis le LAN (Wi‑Fi, partage téléphone, Ethernet).
    Lan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ForwardPid {
    host_port: u16,
    guest_port: u16,
    bind: PortBind,
    pid: u32,
}

fn pid_file(instance_dir: &Path) -> PathBuf {
    instance_dir.join(FORWARD_PID_FILE)
}

fn listen_bind_address(bind: PortBind) -> &'static str {
    match bind {
        PortBind::Local => "127.0.0.1",
        PortBind::Lan => "0.0.0.0",
    }
}

/// Vérifie que le port hôte n'est pas déjà pris sur l'adresse d'écoute.
pub fn ensure_host_port_available(host_port: u16, bind: PortBind) -> Result<()> {
    use std::net::TcpListener;
    let addr = format!("{}:{host_port}", listen_bind_address(bind));
    TcpListener::bind(&addr).with_context(|| {
        let scope = match bind {
            PortBind::Local => "127.0.0.1",
            PortBind::Lan => "0.0.0.0 (LAN)",
        };
        format!(
            "Le port hôte {host_port} est déjà utilisé sur {scope}. \
             Choisissez un autre port dans le Virtumfile (ex. 18080:80) \
             ou arrêtez le processus qui l'occupe."
        )
    })?;
    Ok(())
}

pub fn ensure_ports_available(forwards: &[PortForward], bind: PortBind) -> Result<()> {
    for pf in forwards {
        ensure_host_port_available(pf.host_port, bind)?;
    }
    Ok(())
}

/// Pas de doublon de port hôte dans un Virtumfile.
pub fn validate_lab_port_uniqueness(machines: &[(String, Vec<PortForward>)]) -> Result<()> {
    let mut seen = HashSet::new();
    for (machine, forwards) in machines {
        for pf in forwards {
            if !seen.insert(pf.host_port) {
                bail!(
                    "Port hôte {} en double dans le Virtumfile (machine '{machine}'). \
                     Chaque mapping doit utiliser un port hôte unique.",
                    pf.host_port
                );
            }
        }
    }
    Ok(())
}

/// Conflit avec une autre instance Virtum (metadata).
pub fn ensure_ports_not_in_metadata(
    metadata: &Metadata,
    skip_vm_id: Option<&str>,
    forwards: &[PortForward],
) -> Result<()> {
    for inst in &metadata.instances {
        if inst.status == InstanceStatus::Destroyed {
            continue;
        }
        if skip_vm_id == Some(inst.id.as_str()) {
            continue;
        }
        let Some(existing) = &inst.port_forwards else {
            continue;
        };
        for pf in forwards {
            for other in existing {
                if other.host_port == pf.host_port {
                    let label = inst.name.as_deref().unwrap_or(&inst.id);
                    bail!(
                        "Le port hôte {} est déjà mappé par l'instance '{label}' ({id}).",
                        pf.host_port,
                        id = inst.id
                    );
                }
            }
        }
    }
    Ok(())
}

fn ensure_socat() -> Result<()> {
    let ok = Command::new("which")
        .arg("socat")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return Ok(());
    }
    bail!(
        "socat est requis pour le mapping de ports (`ports:` dans le Virtumfile). \
         Installez-le : sudo apt install socat"
    );
}

/// Démarre les redirections TCP (processus `socat` détachés).
pub fn start_instance_proxies(
    instance_dir: &Path,
    guest_ip: &str,
    forwards: &[PortForward],
    bind: PortBind,
) -> Result<()> {
    stop_instance_proxies(instance_dir);
    if forwards.is_empty() {
        return Ok(());
    }
    ensure_socat()?;
    ensure_ports_available(forwards, bind)?;

    if bind == PortBind::Lan {
        if let Some(ip) = bridge_network::host_primary_ipv4() {
            eprintln!("Ports exposés sur le LAN (écoute 0.0.0.0) — IP hôte typique : {ip}");
        } else {
            eprintln!(
                "Ports exposés sur le LAN (0.0.0.0) ; impossible de détecter l'IP hôte (ip route)."
            );
        }
        eprintln!(
            "Si un pare-feu bloque l'accès (ufw), autorisez le port ou testez depuis le même réseau."
        );
    }

    let mut records = Vec::new();
    for &pf in forwards {
        let pid = spawn_socat_forward(guest_ip, pf, bind)?;
        records.push(ForwardPid {
            host_port: pf.host_port,
            guest_port: pf.guest_port,
            bind,
            pid,
        });
    }

    let path = pid_file(instance_dir);
    let content = serde_json::to_string_pretty(&records).context("sérialisation port-forwards")?;
    std::fs::write(&path, format!("{content}\n"))
        .with_context(|| format!("Impossible d'écrire {}", path.display()))?;
    Ok(())
}

fn spawn_socat_forward(guest_ip: &str, pf: PortForward, bind: PortBind) -> Result<u32> {
    let listen_addr = listen_bind_address(bind);
    let listen = format!(
        "TCP-LISTEN:{},bind={listen_addr},reuseaddr,fork",
        pf.host_port
    );
    let target = format!("TCP:{guest_ip}:{}", pf.guest_port);
    let child = Command::new("socat")
        .arg(&listen)
        .arg(&target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "Impossible de lancer socat pour {listen_addr}:{} → {guest_ip}:{}",
                pf.host_port, pf.guest_port
            )
        })?;
    Ok(child.id())
}

/// Arrête les processus socat enregistrés pour cette instance.
pub fn stop_instance_proxies(instance_dir: &Path) {
    let path = pid_file(instance_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Ok(records) = serde_json::from_str::<Vec<ForwardPid>>(&content) {
        for record in records {
            signal_pid(record.pid);
        }
    }
    let _ = std::fs::remove_file(&path);
}

fn signal_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn format_forward_list(forwards: &[PortForward], bind: PortBind) -> String {
    if forwards.is_empty() {
        return "-".to_string();
    }
    let host_ip = bridge_network::host_primary_ipv4();
    forwards
        .iter()
        .map(|pf| match bind {
            PortBind::Local => format!("127.0.0.1:{}→{}", pf.host_port, pf.guest_port),
            PortBind::Lan => {
                if let Some(ref ip) = host_ip {
                    format!("{ip}:{}→{}", pf.host_port, pf.guest_port)
                } else {
                    format!("LAN:{}→{}", pf.host_port, pf.guest_port)
                }
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn print_forward_lines(forwards: &[PortForward], bind: PortBind) {
    let lan_ip = bridge_network::host_primary_ipv4();
    for pf in forwards {
        println!(
            "  Forward (local): http://127.0.0.1:{} → invité :{}",
            pf.host_port, pf.guest_port
        );
        if bind == PortBind::Lan {
            if let Some(ref ip) = lan_ip {
                println!(
                    "  Forward (LAN):   http://{ip}:{} → invité :{}  (téléphone / collègue même réseau)",
                    pf.host_port, pf.guest_port
                );
            } else {
                println!(
                    "  Forward (LAN):   http://<IP-hôte>:{} → invité :{}  (écoute 0.0.0.0, voir `ip route`)",
                    pf.host_port, pf.guest_port
                );
            }
        }
    }
}

/// Virtumfile modifié alors que la VM existe déjà (sans recréer).
pub fn ensure_forwards_match_existing(
    machine_name: &str,
    spec_ports: &[PortForward],
    spec_bind: PortBind,
    existing: &Instance,
) -> Result<()> {
    let stored = existing.port_forwards.as_deref().unwrap_or(&[]);
    let stored_bind = existing.port_bind.unwrap_or(PortBind::Local);
    if stored == spec_ports && stored_bind == spec_bind {
        return Ok(());
    }
    if stored.is_empty() && !spec_ports.is_empty() {
        bail!(
            "La machine '{machine_name}' existe sans mapping de ports. \
             Utilisez `virtum apply -f <Virtumfile>` ou `virtum down --destroy` puis `virtum up`."
        );
    }
    if !stored.is_empty() && spec_ports.is_empty() {
        bail!(
            "La machine '{machine_name}' a des ports mappés mais le Virtumfile n'en définit plus. \
             Utilisez `virtum apply -f <Virtumfile>` ou `virtum down --destroy` puis `virtum up`."
        );
    }
    bail!(
        "Les ports ou `bind` du Virtumfile ne correspondent pas à l'instance '{machine_name}' ({id}). \
         Utilisez `virtum apply -f <Virtumfile>`.",
        id = existing.id
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_bind_address_local_and_lan() {
        assert_eq!(listen_bind_address(PortBind::Local), "127.0.0.1");
        assert_eq!(listen_bind_address(PortBind::Lan), "0.0.0.0");
    }

    #[test]
    fn validate_lab_port_uniqueness_rejects_duplicate() {
        let machines = vec![
            (
                "web".into(),
                vec![PortForward {
                    host_port: 18080,
                    guest_port: 80,
                }],
            ),
            (
                "api".into(),
                vec![PortForward {
                    host_port: 18080,
                    guest_port: 8080,
                }],
            ),
        ];
        assert!(validate_lab_port_uniqueness(&machines).is_err());
    }
}
