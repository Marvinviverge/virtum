//! `virtum ps` — liste des VM (équivalent à `docker ps`).

use anyhow::{Context, Result};

use std::path::Path;

use crate::config::{self, VirtumPaths};
use crate::disk::{format_virtual_size_label, image_virtual_size_bytes};
use crate::libvirt::{domain_exists, domain_state};
use crate::metadata::{self, Instance, InstanceStatus};
use crate::port_forward;
use crate::tr;

#[derive(Debug, Clone)]
struct PsRow {
    name: String,
    lab: String,
    id: String,
    os: String,
    status: String,
    disk: String,
    ip: String,
    lan_ip: String,
    forward: String,
}

pub fn ps(all: bool) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context(tr!(
            fr: "Impossible de démarrer le runtime Tokio",
            en: "Cannot start the Tokio runtime"
        ))?
        .block_on(ps_async(all))
}

async fn ps_async(all: bool) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    let mut rows: Vec<PsRow> = Vec::new();
    for instance in &metadata.instances {
        if instance.status == InstanceStatus::Destroyed {
            continue;
        }
        let live = live_status(&instance.id, instance.status);
        if !all && !is_active_live(&live) {
            continue;
        }
        rows.push(PsRow {
            name: instance.name.clone().unwrap_or_else(|| "-".to_string()),
            lab: instance.lab_name.clone().unwrap_or_else(|| "-".to_string()),
            id: instance.id.clone(),
            os: instance.os.clone(),
            status: live,
            disk: resolve_disk_size(instance, &paths),
            ip: format_ip(instance),
            lan_ip: format_lan_ip(instance),
            forward: port_forward::format_forward_list(
                instance.port_forwards.as_deref().unwrap_or(&[]),
                instance.port_bind.unwrap_or(port_forward::PortBind::Local),
            ),
        });
    }

    print_table(&rows);
    if !all {
        if let Some(hint) = stopped_lab_hint(&metadata.instances, |id| {
            domain_exists(id) && !is_active_live(&domain_state(id).unwrap_or_default())
        }) {
            eprintln!("{hint}");
        }
    }
    Ok(())
}

/// Signale les VM de lab éteintes (cas typique : hôte redémarré), absentes de `virtum ps`.
fn stopped_lab_hint(instances: &[Instance], is_stopped: impl Fn(&str) -> bool) -> Option<String> {
    let mut by_lab: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for instance in instances
        .iter()
        .filter(|i| i.status != InstanceStatus::Destroyed)
    {
        let Some(lab) = instance.lab_name.as_deref() else {
            continue;
        };
        if is_stopped(&instance.id) {
            by_lab
                .entry(lab)
                .or_default()
                .push(instance.name.as_deref().unwrap_or(&instance.id));
        }
    }
    if by_lab.is_empty() {
        return None;
    }
    let labs: Vec<String> = by_lab
        .iter()
        .map(|(lab, vms)| {
            let vms = vms.join(", ");
            tr!(fr: "lab '{lab}' : {vms}", en: "lab '{lab}': {vms}")
        })
        .collect();
    let labs = labs.join(&tr!(fr: " ; ", en: "; "));
    Some(tr!(
        fr: "VM éteintes ({labs}). Relancez avec `virtum up` dans le dossier du lab ; détail : `virtum ps -a`.",
        en: "Stopped VMs ({labs}). Restart them with `virtum up` in the lab folder; details: `virtum ps -a`."
    ))
}

fn live_status(vm_id: &str, metadata_status: InstanceStatus) -> String {
    if domain_exists(vm_id) {
        return domain_state(vm_id).unwrap_or_else(|_| "unknown".to_string());
    }
    match metadata_status {
        InstanceStatus::Running => "running".to_string(),
        InstanceStatus::Stopped => "shut off".to_string(),
        InstanceStatus::Creating => "creating".to_string(),
        InstanceStatus::Destroyed => "destroyed".to_string(),
    }
}

fn is_active_live(status: &str) -> bool {
    matches!(status, "running" | "paused" | "pmsuspended" | "blocked")
}

fn resolve_disk_size(instance: &Instance, paths: &VirtumPaths) -> String {
    if let Some(ref label) = instance.disk_size {
        if !label.is_empty() {
            return label.clone();
        }
    }
    let disk_path = paths.instances.join(&instance.id).join("disk.qcow2");
    disk_size_from_path(&disk_path).unwrap_or_else(|| "-".to_string())
}

fn disk_size_from_path(disk_path: &Path) -> Option<String> {
    if !disk_path.is_file() {
        return None;
    }
    image_virtual_size_bytes(disk_path)
        .ok()
        .map(format_virtual_size_label)
}

fn format_lan_ip(instance: &Instance) -> String {
    match &instance.lan_ip {
        Some(ip) if !ip.is_empty() => ip.clone(),
        _ => "-".to_string(),
    }
}

fn format_ip(instance: &Instance) -> String {
    let Some(ref ip) = instance.ip else {
        return "-".to_string();
    };
    if ip.is_empty() {
        return "-".to_string();
    }
    if let Some(port) = instance.ssh_port {
        if port != 22 {
            return format!("{ip}:{port}");
        }
    }
    ip.clone()
}

fn print_table(rows: &[PsRow]) {
    println!(
        "{:<10} {:<16} {:<18} {:<12} {:<12} {:<8} {:<18} {:<18} {:<28}",
        "NAME",    // i18n-ok : en-tête lu par des scripts
        "LAB",     // i18n-ok : en-tête lu par des scripts
        "VM ID",   // i18n-ok : en-tête lu par des scripts
        "OS",      // i18n-ok : en-tête lu par des scripts
        "STATUS",  // i18n-ok : en-tête lu par des scripts
        "DISK",    // i18n-ok : en-tête lu par des scripts
        "IP",      // i18n-ok : en-tête lu par des scripts
        "LAN",     // i18n-ok : en-tête lu par des scripts
        "FORWARD"  // i18n-ok : en-tête lu par des scripts
    );
    for row in rows {
        println!(
            "{:<10} {:<16} {:<18} {:<12} {:<12} {:<8} {:<18} {:<18} {:<28}",
            row.name,
            row.lab,
            row.id,
            row.os,
            row.status,
            row.disk,
            row.ip,
            row.lan_ip,
            row.forward
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab_instance(id: &str, name: &str, lab: Option<&str>) -> Instance {
        let mut instance: Instance = serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"{name}","os":"ubuntu-24.04","status":"running",
                "user":"virtum","hostname":"{name}","created_at":"2026-10-29T09:00:00Z"}}"#
        ))
        .unwrap();
        instance.lab_name = lab.map(str::to_string);
        instance
    }

    #[test]
    fn stopped_lab_hint_groups_by_lab() {
        let instances = vec![
            lab_instance("vm-1", "app", Some("infra-local")),
            lab_instance("vm-2", "edge", Some("infra-local")),
            lab_instance("vm-3", "web", Some("demo")),
            lab_instance("vm-4", "solo", None),
        ];
        let hint = stopped_lab_hint(&instances, |id| id != "vm-3").unwrap();
        assert!(hint.contains("lab 'infra-local' : app, edge"));
        assert!(!hint.contains("web"));
        assert!(!hint.contains("solo"), "VM hors lab ignorée");
        assert!(hint.contains("virtum up"));
        assert!(stopped_lab_hint(&instances, |_| false).is_none());
    }

    #[test]
    fn is_active_live_matches_running_states() {
        assert!(is_active_live("running"));
        assert!(!is_active_live("shut off"));
    }

    #[test]
    fn format_ip_with_port_forward() {
        let inst = Instance {
            id: "vm-test".into(),
            name: None,
            os: "debian-12".into(),
            status: InstanceStatus::Running,
            ip: Some("127.0.0.1".into()),
            ssh_port: Some(22422),
            user: "virtum".into(),
            hostname: "vm-test".into(),
            disk_size: Some("10G".into()),
            lab_name: None,
            machine_name: None,
            port_forwards: None,
            port_bind: None,
            bridge_host_interface: None,
            lan_ip: None,
            mounts: None,
            restart: None,
            static_ip: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert_eq!(format_ip(&inst), "127.0.0.1:22422");
    }

    #[test]
    fn resolve_disk_size_prefers_metadata() {
        let paths = config::VirtumPaths {
            root: std::path::PathBuf::from("/tmp"),
            images: std::path::PathBuf::from("/tmp/images"),
            instances: std::path::PathBuf::from("/tmp/instances"),
            keys: std::path::PathBuf::from("/tmp/keys"),
            metadata: std::path::PathBuf::from("/tmp/metadata.json"),
        };
        let inst = Instance {
            id: "vm-x".into(),
            name: None,
            os: "debian-12".into(),
            status: InstanceStatus::Running,
            ip: None,
            ssh_port: None,
            user: "virtum".into(),
            hostname: "vm-x".into(),
            disk_size: Some("20G".into()),
            lab_name: Some("my-lab".into()),
            machine_name: Some("web".into()),
            port_forwards: None,
            port_bind: None,
            bridge_host_interface: None,
            lan_ip: None,
            mounts: None,
            restart: None,
            static_ip: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert_eq!(super::resolve_disk_size(&inst, &paths), "20G");
    }
}
