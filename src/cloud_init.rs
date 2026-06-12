//! cloud-init user-data, meta-data and ISO generation (steps 11–12).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::command::{path_arg, run_command};
use crate::mounts::ResolvedMount;

/// `/etc/profile.d` : PATH incluant `/usr/sbin` (nginx, ip, etc.) pour les shells interactifs.
const PATH_PROFILE_SNIPPET: &str = r#"
write_files:
  - path: /etc/profile.d/virtum-path.sh
    permissions: '0644'
    content: |
      # Virtum — PATH Debian complet pour sessions SSH interactives
      case ":${PATH}:" in
        *:/usr/sbin:*) ;;
        *) export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" ;;
      esac
"#;

/// Writes `user-data` (cloud-config) with user, sudo and Virtum SSH public key.
///
/// Si `grow_root` est vrai, cloud-init agrandit la partition racine au premier boot.
/// Les commandes `provision` ne sont pas injectées ici : exécutées via SSH après
/// que la VM accepte les connexions (voir `ssh::run_provision_commands`).
fn mount_runcmd_block(mounts: &[ResolvedMount]) -> String {
    if mounts.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for m in mounts {
        lines.push(format!("  - [ mkdir, -p, {} ]", m.guest_path));
        lines.push(format!(
            "  - [ mount, -t, virtiofs, {}, {} ]",
            m.name, m.guest_path
        ));
        lines.push(format!(
            "  - [ chown, -R, virtum:virtum, {} ]",
            m.guest_path
        ));
    }
    format!("runcmd:\n{}\n", lines.join("\n"))
}

pub fn write_user_data(
    instance_dir: &Path,
    user: &str,
    public_key: &str,
    grow_root: bool,
    mounts: &[ResolvedMount],
) -> Result<PathBuf> {
    let path = instance_dir.join("user-data");
    let grow_block = if grow_root {
        r"
growpart:
  mode: auto
  devices: ['/']
resize_rootfs: true
"
    } else {
        ""
    };
    let mount_block = mount_runcmd_block(mounts);
    let (package_block, runcmd_block) = if grow_root {
        // Pas de runcmd/apt au boot : growpart d'abord ; mounts via runcmd dédié si présents.
        (String::new(), mount_block)
    } else if mounts.is_empty() {
        (
            r"package_update: true
packages:
  - qemu-guest-agent
"
            .to_string(),
            "runcmd:\n  - [ systemctl, enable, --now, qemu-guest-agent ]\n".to_string(),
        )
    } else {
        let mut runcmd =
            String::from("runcmd:\n  - [ systemctl, enable, --now, qemu-guest-agent ]\n");
        for line in mount_block.lines().skip(1) {
            runcmd.push_str(line);
            runcmd.push('\n');
        }
        (
            r"package_update: true
packages:
  - qemu-guest-agent
"
            .to_string(),
            runcmd,
        )
    };
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
{path_profile}{package_block}{grow_block}{runcmd_block}"#,
        path_profile = PATH_PROFILE_SNIPPET,
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
        write_user_data(&dir, "virtum", key, false, &[]).unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.starts_with("#cloud-config\n"));
        assert!(content.contains("  - name: virtum\n"));
        assert!(content.contains(&format!("      - {key}")));
        assert!(content.contains("ssh_pwauth: false"));
        assert!(content.contains("/etc/profile.d/virtum-path.sh"));
        assert!(content.contains("package_update: true"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_data_grow_root_has_no_runcmd() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-grow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_user_data(&dir, "virtum", "ssh-ed25519 AAAATEST virtum", true, &[]).unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.contains("growpart:"));
        assert!(content.contains("resize_rootfs: true"));
        assert!(!content.contains("runcmd:"));
        assert!(!content.contains("packages:"));
        assert!(content.contains("/etc/profile.d/virtum-path.sh"));

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

    #[test]
    fn user_data_includes_mount_runcmd() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-mount-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mounts = vec![crate::mounts::ResolvedMount {
            name: "app".into(),
            host: "./app".into(),
            guest: "~/app".into(),
            host_abs: dir.join("app"),
            guest_path: "/home/virtum/app".into(),
            socket_path: dir.join("virtiofs-app.sock"),
            virtiofsd_pid: None,
        }];
        write_user_data(
            &dir,
            "virtum",
            "ssh-ed25519 AAAATEST virtum",
            false,
            &mounts,
        )
        .unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.contains("/home/virtum/app"));
        assert!(content.contains("virtiofs"));
        assert!(content.contains("app"));
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

        write_user_data(&dir, "virtum", "ssh-ed25519 AAAATEST virtum", false, &[]).unwrap();
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
