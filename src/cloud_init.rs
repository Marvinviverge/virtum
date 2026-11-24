//! cloud-init user-data, meta-data and ISO generation (steps 11–12).

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::command::{path_arg, run_command};
use crate::mounts::ResolvedMount;
use crate::ssh::GuestHostKey;
use crate::tr;

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

pub fn validate_cloud_init_user(user: &str) -> Result<()> {
    if user.is_empty() || user.len() > 32 {
        bail!(tr!(
            fr: "Utilisateur cloud-init invalide : longueur 1–32 requise.",
            en: "Invalid cloud-init user: length must be 1–32."
        ));
    }
    let mut chars = user.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        bail!(tr!(
            fr: "Utilisateur cloud-init invalide : doit commencer par [a-z_].",
            en: "Invalid cloud-init user: must start with [a-z_]."
        ));
    }
    if !user
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(tr!(
            fr: "Utilisateur cloud-init invalide : caractères autorisés [a-z0-9_-].",
            en: "Invalid cloud-init user: allowed characters are [a-z0-9_-]."
        ));
    }
    Ok(())
}

pub fn validate_cloud_init_hostname(hostname: &str) -> Result<()> {
    if hostname.is_empty() || hostname.len() > 64 {
        bail!(tr!(
            fr: "Hostname cloud-init invalide : longueur 1–64 requise.",
            en: "Invalid cloud-init hostname: length must be 1–64."
        ));
    }
    if !hostname
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        bail!(tr!(
            fr: "Hostname cloud-init invalide : caractères autorisés [a-zA-Z0-9_.-].",
            en: "Invalid cloud-init hostname: allowed characters are [a-zA-Z0-9_.-]."
        ));
    }
    Ok(())
}

pub fn validate_cloud_init_vm_id(vm_id: &str) -> Result<()> {
    if vm_id.is_empty() {
        bail!(tr!(
            fr: "Identifiant instance cloud-init vide.",
            en: "Empty cloud-init instance ID."
        ));
    }
    if !vm_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!(tr!(
            fr: "Identifiant instance '{vm_id}' invalide (attendu alphanum + '-').",
            en: "Invalid instance ID '{vm_id}' (expected alphanumerics and '-')."
        ));
    }
    Ok(())
}

fn mount_runcmd_entries(mounts: &[ResolvedMount]) -> Vec<Vec<String>> {
    let mut entries = Vec::new();
    for m in mounts {
        entries.push(vec!["mkdir".into(), "-p".into(), m.guest_path.clone()]);
        let mut mount: Vec<String> = vec!["mount".into(), "-t".into(), "virtiofs".into()];
        if m.read_only {
            mount.extend(["-o".into(), "ro".into()]);
        }
        mount.extend([m.name.clone(), m.guest_path.clone()]);
        entries.push(mount);
        // Lecture seule : l'hôte refuse toute écriture, chown compris.
        if !m.read_only {
            entries.push(vec![
                "chown".into(),
                "-R".into(),
                "virtum:virtum".into(),
                m.guest_path.clone(),
            ]);
        }
    }
    entries
}

fn yaml_runcmd_block(entries: &[Vec<String>]) -> String {
    #[derive(serde::Serialize)]
    struct Runcmd<'a> {
        runcmd: &'a [Vec<String>],
    }
    if entries.is_empty() {
        return String::new();
    }
    format!(
        "{}\n",
        serde_saphyr::to_string(&Runcmd { runcmd: entries }).expect("runcmd yaml")
    )
}

/// Writes `user-data` (cloud-config) with user, sudo and Virtum SSH public key.
///
/// Si `grow_root` est vrai, cloud-init agrandit la partition racine au premier boot.
/// `install_agent` : installer `qemu-guest-agent` (cartes bridge seulement : sur un réseau
/// libvirt, l'IP vient des baux DHCP ; l'installation coûtait `apt update` + paquet, soit
/// 25 à 40 s par VM, v0.9). Avec `grow_root`, il est installé après SSH (voir `vm.rs`).
/// Les commandes `provision` ne sont pas injectées ici : exécutées via SSH après
/// que la VM accepte les connexions (voir `ssh::run_provision_commands`).
pub fn write_user_data(
    instance_dir: &Path,
    user: &str,
    public_key: &str,
    grow_root: bool,
    install_agent: bool,
    mounts: &[ResolvedMount],
    host_key: Option<&GuestHostKey>,
) -> Result<PathBuf> {
    validate_cloud_init_user(user)?;
    if public_key.contains('\n') {
        bail!(tr!(
            fr: "Clé publique SSH cloud-init invalide : retour ligne interdit.",
            en: "Invalid cloud-init SSH public key: line breaks are not allowed."
        ));
    }
    let host_key_block = match host_key {
        Some(key) => host_key_block(key)?,
        None => String::new(),
    };

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
    let mount_entries = mount_runcmd_entries(mounts);
    let mount_block = yaml_runcmd_block(&mount_entries);
    let (package_block, runcmd_block) = if grow_root || !install_agent {
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
        let mut entries = vec![vec![
            "systemctl".into(),
            "enable".into(),
            "--now".into(),
            "qemu-guest-agent".into(),
        ]];
        entries.extend(mount_entries);
        (
            r"package_update: true
packages:
  - qemu-guest-agent
"
            .to_string(),
            yaml_runcmd_block(&entries),
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
{host_key_block}{path_profile}{package_block}{grow_block}{runcmd_block}"#,
        path_profile = PATH_PROFILE_SNIPPET,
    );
    crate::fsutil::write_atomic(&path, content.as_bytes(), 0o600)?;
    Ok(path)
}

/// Clé d'hôte imposée par Virtum : cloud-init remplace les clés de l'image par celle-ci et
/// n'en génère aucune autre, si bien que la VM ne présente que la clé épinglée.
fn host_key_block(key: &GuestHostKey) -> Result<String> {
    if key.public_key.contains('\n')
        || key
            .private_key
            .lines()
            .any(|l| l.trim().is_empty() || l != l.trim())
    {
        bail!(tr!(
            fr: "Clé d'hôte SSH cloud-init invalide.",
            en: "Invalid cloud-init SSH host key."
        ));
    }
    let mut block = String::from(
        "ssh_deletekeys: true\nssh_genkeytypes: []\nssh_keys:\n  ed25519_private: |\n",
    );
    for line in key.private_key.lines() {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
    }
    block.push_str(&format!("  ed25519_public: {}\n", key.public_key));
    Ok(block)
}

/// Writes `meta-data` with instance-id and hostname.
pub fn write_meta_data(instance_dir: &Path, vm_id: &str, hostname: &str) -> Result<PathBuf> {
    validate_cloud_init_vm_id(vm_id)?;
    validate_cloud_init_hostname(hostname)?;
    let path = instance_dir.join("meta-data");
    let content = format!("instance-id: {vm_id}\nlocal-hostname: {hostname}\n");
    crate::fsutil::write_atomic(&path, content.as_bytes(), 0o600)?;
    Ok(path)
}

/// Builds `cloud-init.iso` via `cloud-localds` (user-data and meta-data kept for debug).
pub fn build_cloud_init_iso(instance_dir: &Path) -> Result<PathBuf> {
    let user_data = instance_dir.join("user-data");
    let meta_data = instance_dir.join("meta-data");
    let iso_path = instance_dir.join("cloud-init.iso");

    for path in [&user_data, &meta_data] {
        if !path.is_file() {
            let path = path.display();
            anyhow::bail!(tr!(
                fr: "Fichier cloud-init manquant: {path}. \
                     Générez user-data et meta-data d'abord.",
                en: "Missing cloud-init file: {path}. \
                     Generate user-data and meta-data first."
            ));
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
        err.context(tr!(
            fr: "cloud-localds a échoué. Installez cloud-image-utils ou exécutez `virtum doctor`.",
            en: "cloud-localds failed. Install cloud-image-utils or run `virtum doctor`."
        ))
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
        write_user_data(&dir, "virtum", key, false, false, &[], None).unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.starts_with("#cloud-config\n"));
        assert!(content.contains("  - name: virtum\n"));
        assert!(content.contains(&format!("      - {key}")));
        assert!(content.contains("ssh_pwauth: false"));
        assert!(content.contains("/etc/profile.d/virtum-path.sh"));
        // Réseau libvirt : ni apt ni qemu-guest-agent (v0.9).
        assert!(!content.contains("package_update"));
        assert!(!content.contains("qemu-guest-agent"));

        // Carte bridge : l'agent sert à trouver l'IP, il est installé par cloud-init.
        write_user_data(&dir, "virtum", key, false, true, &[], None).unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.contains("package_update: true"));
        assert!(content.contains("  - qemu-guest-agent"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_data_grow_root_has_no_runcmd() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-grow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_user_data(
            &dir,
            "virtum",
            "ssh-ed25519 AAAATEST virtum",
            true,
            true,
            &[],
            None,
        )
        .unwrap();
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
            virtumfile_dir: None,
            read_only: false,
        }];
        write_user_data(
            &dir,
            "virtum",
            "ssh-ed25519 AAAATEST virtum",
            false,
            false,
            &mounts,
            None,
        )
        .unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.contains("/home/virtum/app"));
        assert!(content.contains("virtiofs"));
        assert!(content.contains("app"));
        assert!(!content.contains("mkdir -p /home"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// cloud-init lit le YAML en 1.1 (PyYAML) : `yes`, `on`, `0755`, `~`… doivent rester
    /// des chaînes, donc être entre guillemets.
    #[test]
    fn runcmd_quotes_yaml11_scalars() {
        let row: Vec<String> = ["yes", "on", "no", "0755", "~", "null", "1e3", "true"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = yaml_runcmd_block(std::slice::from_ref(&row));
        for v in &row {
            assert!(out.contains(&format!("\"{v}\"")), "{v} non quoté :\n{out}");
        }
        let back: serde_json::Value = serde_saphyr::from_str(&out).unwrap();
        assert_eq!(back["runcmd"][0], serde_json::json!(row));
    }

    #[test]
    fn rejects_unsafe_cloud_init_fields() {
        assert!(validate_cloud_init_user("bad user").is_err());
        assert!(validate_cloud_init_hostname("host\nname").is_err());
        assert!(validate_cloud_init_vm_id("vm;id").is_err());
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

        write_user_data(
            &dir,
            "virtum",
            "ssh-ed25519 AAAATEST virtum",
            false,
            false,
            &[],
            None,
        )
        .unwrap();
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

    #[test]
    fn user_data_imposes_pinned_host_key() {
        let dir = std::env::temp_dir().join(format!("virtum-ci-hostkey-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let key = GuestHostKey {
            private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----\n".into(),
            public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHost".into(),
        };
        write_user_data(
            &dir,
            "virtum",
            "ssh-ed25519 AAAAUser virtum",
            false,
            false,
            &[],
            Some(&key),
        )
        .unwrap();
        let content = fs::read_to_string(dir.join("user-data")).unwrap();
        assert!(content.contains(
            "ssh_deletekeys: true\nssh_genkeytypes: []\nssh_keys:\n  ed25519_private: |\n    -----BEGIN OPENSSH PRIVATE KEY-----\n    b3BlbnNzaA==\n    -----END OPENSSH PRIVATE KEY-----\n  ed25519_public: ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHost\n"
        ));
        let value: serde_json::Value = serde_saphyr::from_str(&content).unwrap();
        assert_eq!(value["ssh_genkeytypes"], serde_json::json!([]));
        assert!(value["ssh_keys"]["ed25519_private"]
            .as_str()
            .unwrap()
            .ends_with("-----END OPENSSH PRIVATE KEY-----\n"));

        // Une clé qui casserait le YAML est refusée.
        let bad = GuestHostKey {
            private_key: "-----BEGIN\n\nx".into(),
            public_key: key.public_key.clone(),
        };
        assert!(write_user_data(
            &dir,
            "virtum",
            "ssh-ed25519 AAAAUser",
            false,
            false,
            &[],
            Some(&bad)
        )
        .is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
