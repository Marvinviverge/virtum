//! Dedicated SSH key under `~/.virtum/keys/` (step 7).

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::command::{path_arg, run_command};
use crate::config;
use crate::libvirt::SLIRP_GUEST_IPV4;
use crate::metadata::{self, InstanceStatus};

const PRIVATE_KEY_NAME: &str = "virtum_id_ed25519";
const PUBLIC_KEY_NAME: &str = "virtum_id_ed25519.pub";

#[derive(Debug, Clone)]
pub struct SshKeyPaths {
    pub private_key: PathBuf,
    pub public_key: PathBuf,
}

/// Creates the Virtum ed25519 key pair if missing; returns paths to private and public keys.
pub fn ensure_ssh_keypair(keys_dir: &Path) -> Result<SshKeyPaths> {
    fs::create_dir_all(keys_dir)
        .with_context(|| format!("Impossible de créer {}", keys_dir.display()))?;

    let private_key = keys_dir.join(PRIVATE_KEY_NAME);
    let public_key = keys_dir.join(PUBLIC_KEY_NAME);

    if private_key.is_file() && public_key.is_file() {
        restrict_private_key_permissions(&private_key)?;
        return Ok(SshKeyPaths {
            private_key,
            public_key,
        });
    }

    let private = private_key.to_string_lossy();
    run_command(
        "ssh-keygen",
        &[
            "-t",
            "ed25519",
            "-f",
            private.as_ref(),
            "-N",
            "",
            "-C",
            "virtum",
        ],
    )?;

    restrict_private_key_permissions(&private_key)?;

    Ok(SshKeyPaths {
        private_key,
        public_key,
    })
}

/// Reads the public key as a single line suitable for cloud-init `ssh_authorized_keys`.
pub fn read_public_key(public_key_path: &Path) -> Result<String> {
    let content = fs::read_to_string(public_key_path)
        .with_context(|| format!("Impossible de lire {}", public_key_path.display()))?;
    let line = content.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        anyhow::bail!("Clé publique vide dans {}", public_key_path.display());
    }
    Ok(line.to_string())
}

/// SSH interactif vers une instance (étape 17) ; termine le processus avec le code SSH.
pub fn shell_to_instance(vm_id: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?;

    let (private_key, user, host, port) = runtime.block_on(async {
        let paths = config::ensure_layout().await?;
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        let instance = metadata::find_instance(&metadata, vm_id).ok_or_else(|| {
            anyhow::anyhow!(
                "Instance '{vm_id}' introuvable dans metadata.json. \
                 Lancez `virtum run` ou vérifiez l'identifiant."
            )
        })?;

        if instance.status != InstanceStatus::Running {
            let hint = match instance.status {
                InstanceStatus::Stopped => {
                    format!("Utilisez `virtum start {vm_id}` pour la relancer.")
                }
                _ => String::new(),
            };
            bail!(
                "La VM '{vm_id}' n'est pas en cours d'exécution (statut: {:?}). {hint}",
                instance.status
            );
        }

        let ip = instance
            .ip
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Aucune adresse IP enregistrée pour '{vm_id}'. \
                     Utilisez `virtum start {vm_id}` ou `virtum run`."
                )
            })?;

        let port = instance.ssh_port.unwrap_or(22);

        if (ip == "127.0.0.1" || ip == SLIRP_GUEST_IPV4) && instance.ssh_port.is_some() {
            eprintln!(
                "Avertissement: instance '{vm_id}' utilise l'ancien mode SLIRP (127.0.0.1:{port}). \
                 Relancez `virtum run` pour une IP dédiée (192.168.122.x)."
            );
        } else if ip == SLIRP_GUEST_IPV4 {
            bail!(
                "La VM '{vm_id}' a l'IP SLIRP {SLIRP_GUEST_IPV4}, inaccessible depuis l'hôte. \
                 Recréez-la avec `virtum run` (réseau libvirt default)."
            );
        }

        let host = ip;

        let keys = ensure_ssh_keypair(&paths.keys)?;
        Ok((
            keys.private_key,
            instance.user.clone(),
            host.to_string(),
            port,
        ))
    })?;

    exec_ssh_shell(&private_key, &user, &host, port)
}

/// Vérifie que l'authentification par clé Virtum fonctionne (cloud-init terminé).
pub fn ssh_auth_probe(private_key: &Path, user: &str, host: &str, port: u16) -> bool {
    let key = match path_arg(private_key) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let target = format!("{user}@{host}");
    let mut args = vec![
        "-i",
        key,
        "-o",
        "BatchMode=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
    ];
    let port_str = port.to_string();
    if port != 22 {
        args.push("-p");
        args.push(&port_str);
    }
    args.push(&target);
    args.push("true");

    std::process::Command::new("ssh")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn exec_ssh_shell(private_key: &Path, user: &str, host: &str, port: u16) -> Result<()> {
    let key = path_arg(private_key)?;
    let target = format!("{user}@{host}");

    let mut cmd = Command::new("ssh");
    cmd.arg("-i")
        .arg(key)
        .arg("-o")
        .arg("IdentitiesOnly=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null");
    if port != 22 {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(&target)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("Impossible d'exécuter ssh vers {target}"))?;

    std::process::exit(status.code().unwrap_or(1));
}

fn restrict_private_key_permissions(private_key: &Path) -> Result<()> {
    fs::set_permissions(private_key, Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "Impossible de définir les permissions 0600 sur {}",
            private_key.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_ssh_keypair_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("virtum-ssh-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let paths1 = ensure_ssh_keypair(&dir).unwrap();
        let paths2 = ensure_ssh_keypair(&dir).unwrap();
        assert_eq!(paths1.private_key, paths2.private_key);
        assert!(paths1.private_key.is_file());
        assert!(paths1.public_key.is_file());

        let mode = fs::metadata(&paths1.private_key)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let pubkey = read_public_key(&paths1.public_key).unwrap();
        assert!(pubkey.starts_with("ssh-ed25519 "));

        let _ = fs::remove_dir_all(&dir);
    }
}
