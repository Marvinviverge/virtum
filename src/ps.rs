//! `virtum ps` — liste des VM (équivalent à `docker ps`).

use anyhow::{Context, Result};

use crate::config;
use crate::libvirt::{domain_exists, domain_state};
use crate::metadata::{self, Instance, InstanceStatus};

#[derive(Debug, Clone)]
struct PsRow {
    id: String,
    os: String,
    status: String,
    ip: String,
}

pub fn ps(all: bool) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
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
            id: instance.id.clone(),
            os: instance.os.clone(),
            status: live,
            ip: format_ip(instance),
        });
    }

    print_table(&rows);
    Ok(())
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
    println!("{:<18} {:<12} {:<12} {:<24}", "VM ID", "OS", "STATUS", "IP");
    for row in rows {
        println!(
            "{:<18} {:<12} {:<12} {:<24}",
            row.id, row.os, row.status, row.ip
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_active_live_matches_running_states() {
        assert!(is_active_live("running"));
        assert!(!is_active_live("shut off"));
    }

    #[test]
    fn format_ip_with_port_forward() {
        let inst = Instance {
            id: "vm-test".into(),
            os: "debian-12".into(),
            status: InstanceStatus::Running,
            ip: Some("127.0.0.1".into()),
            ssh_port: Some(22422),
            user: "virtum".into(),
            hostname: "vm-test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert_eq!(format_ip(&inst), "127.0.0.1:22422");
    }
}
