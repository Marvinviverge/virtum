//! cloud-init user-data, meta-data and ISO generation (steps 11–12).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::command::{path_arg, run_command};

/// Writes `user-data` (cloud-config) with user, sudo and Virtum SSH public key.
pub fn write_user_data(instance_dir: &Path, user: &str, public_key: &str) -> Result<PathBuf> {
    let path = instance_dir.join("user-data");
    let content = format!(
        r#"#cloud-config
users:
  - name: {user}
    sudo: ALL=(ALL) NOPASSWD:ALL
    groups: users
    shell: /bin/bash
    lock_passwd: false
    ssh_authorized_keys:
      - {public_key}
ssh_pwauth: false
package_update: true
packages:
  - qemu-guest-agent
runcmd:
  - [ systemctl, enable, --now, qemu-guest-agent ]
"#
    );
    std::fs::write(&path, content)
        .with_context(|| format!("Impossible d'écrire {}", path.display()))?;
    Ok(path)
}

/// Writes `meta-data` with instance-id and hostname.
pub fn write_meta_data(instance_dir: &Path, vm_id: &str, hostname: &str) -> Result<PathBuf> {
    let path = instance_dir.join("meta-data");
    let content = format!("instance-id: {vm_id}\nlocal-hostname: {hostname}\n");
    std::fs::write(&path, content)
        .with_context(|| format!("Impossible d'écrire {}", path.display()))?;
    Ok(path)
}

/// Builds `cloud-init.iso` via `cloud-localds` (user-data and meta-data kept for debug).
pub fn build_cloud_init_iso(instance_dir: &Path) -> Result<PathBuf> {
    let user_data = instance_dir.join("user-data");
    let meta_data = instance_dir.join("meta-data");
    let iso_path = instance_dir.join("cloud-init.iso");

    for path in [&user_data, &meta_data] {
        if !path.is_file() {
            anyhow::bail!(
                "Fichier cloud-init manquant: {}. \
                 Générez user-data et meta-data d'abord.",
                path.display()
            );
        }
    }

    run_command(
        "cloud-localds",
        &[
            path_arg(&iso_path)?,
            path_arg(&user_data)?,
            path_arg(&meta_data)?,
        ],
    )
    .map_err(|err| {
        err.context(
            "cloud-localds a échoué. Installez cloud-image-utils ou exécutez `virtum doctor`.",
        )
    })?;

    Ok(iso_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn user_data_contains_public_key() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKey virtum";
        write_user_data(&dir, "virtum", key).unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.starts_with("#cloud-config\n"));
        assert!(content.contains("  - name: virtum\n"));
        assert!(content.contains(&format!("      - {key}")));
        assert!(content.contains("ssh_pwauth: false"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_data_contains_instance_id() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-meta-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_meta_data(&dir, "vm-abc123", "vm-abc123").unwrap();
        let content = fs::read_to_string(dir.join("meta-data")).unwrap();
        assert!(content.contains("instance-id: vm-abc123"));
        assert!(content.contains("local-hostname: vm-abc123"));

        let _ = fs::remove_dir_all(&dir);
    }

    fn cloud_localds_available() -> bool {
        std::process::Command::new("cloud-localds")
            .arg("--help")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn build_cloud_init_iso_requires_cloud_localds() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-iso-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_user_data(&dir, "virtum", "ssh-ed25519 AAAATEST virtum").unwrap();
        write_meta_data(&dir, "vm-test", "vm-test").unwrap();

        let result = build_cloud_init_iso(&dir);
        if cloud_localds_available() {
            assert!(result.is_ok());
            assert!(dir.join("cloud-init.iso").is_file());
        } else {
            assert!(result.is_err());
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
