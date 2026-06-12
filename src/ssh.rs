//! Dedicated SSH key under `~/.virtum/keys/` (step 7).

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::command::{path_arg, run_command};
use crate::config;
use crate::libvirt::SLIRP_GUEST_IPV4;
use crate::metadata::{self, InstanceStatus};
use crate::mounts::{MountSshStep, ResolvedMount};

const PRIVATE_KEY_NAME: &str = "virtum_id_ed25519";
const PUBLIC_KEY_NAME: &str = "virtum_id_ed25519.pub";

static INSECURE_SSH_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshSecurityMode {
    Secure,
    Insecure,
}

impl SshSecurityMode {
    pub fn from_flag(insecure_ssh: bool) -> Self {
        if insecure_ssh {
            Self::Insecure
        } else {
            Self::Secure
        }
    }
}

/// Options SSH par instance (known_hosts + mode sécurité v0.5).
#[derive(Debug, Clone)]
pub struct SshSessionOpts {
    pub mode: SshSecurityMode,
    pub known_hosts: PathBuf,
}

impl SshSessionOpts {
    pub fn new(instance_dir: &Path, insecure_ssh: bool) -> Self {
        Self {
            mode: SshSecurityMode::from_flag(insecure_ssh),
            known_hosts: known_hosts_path(instance_dir),
        }
    }
}

pub fn known_hosts_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("known_hosts")
}

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

fn warn_insecure_ssh_once() {
    if !INSECURE_SSH_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "Avertissement : --insecure-ssh désactive la vérification de clé SSH \
             (StrictHostKeyChecking=no). À réserver aux scripts / CI de confiance. \
             OpenSSH peut afficher « Permanently added » (écriture dans /dev/null) : bruit sans effet."
        );
    }
}

/// Ignore `~/.ssh/config` : Virtum fournit toutes les options SSH nécessaires.
fn append_ssh_config_isolation(cmd: &mut Command) {
    cmd.arg("-F").arg("/dev/null");
}

fn append_security_opts(cmd: &mut Command, opts: &SshSessionOpts, batch_mode: bool) -> Result<()> {
    append_ssh_config_isolation(cmd);
    if batch_mode {
        cmd.arg("-o").arg("BatchMode=yes");
    }
    cmd.arg("-o").arg("IdentitiesOnly=yes");
    cmd.arg("-o").arg("ConnectTimeout=30");
    match opts.mode {
        SshSecurityMode::Insecure => {
            warn_insecure_ssh_once();
            cmd.arg("-o").arg("StrictHostKeyChecking=no");
            cmd.arg("-o").arg("UserKnownHostsFile=/dev/null");
            cmd.arg("-o").arg("GlobalKnownHostsFile=/dev/null");
        }
        SshSecurityMode::Secure => {
            cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");
            let kh = path_arg(&opts.known_hosts)?;
            cmd.arg("-o").arg(format!("UserKnownHostsFile={kh}"));
            cmd.arg("-o").arg("GlobalKnownHostsFile=/dev/null");
        }
    }
    Ok(())
}

fn map_ssh_host_key_error(stderr: &str, opts: &SshSessionOpts) -> Option<String> {
    if opts.mode == SshSecurityMode::Insecure {
        return None;
    }
    let upper = stderr.to_uppercase();
    if upper.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || upper.contains("HOST KEY VERIFICATION FAILED")
    {
        Some(format!(
            "La clé SSH de la VM a changé. Supprimez {} ou virtum destroy <ref> puis recréez le lab.",
            opts.known_hosts.display()
        ))
    } else {
        None
    }
}

/// SSH interactif vers une instance (étape 17) ; termine le processus avec le code SSH.
pub fn shell_to_instance(reference: &str, insecure_ssh: bool) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?;

    let (private_key, user, host, port, ssh_opts) = runtime.block_on(async {
        let paths = config::ensure_layout().await?;
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        let instance = metadata::resolve_instance_ref(&metadata, reference)?;
        let vm_id = instance.id.as_str();

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
                     Utilisez `virtum start` ou `virtum run`."
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
        let instance_dir = paths.instances.join(vm_id);
        let ssh_opts = SshSessionOpts::new(&instance_dir, insecure_ssh);
        Ok((
            keys.private_key,
            instance.user.clone(),
            host.to_string(),
            port,
            ssh_opts,
        ))
    })?;

    exec_ssh_shell(&private_key, &user, &host, port, &ssh_opts)
}

/// Exécute des commandes de provision via SSH (après premier login).
pub fn run_provision_commands(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    commands: &[String],
    timeout: Duration,
    opts: &SshSessionOpts,
) -> Result<()> {
    use std::time::Instant;

    let started = Instant::now();
    for (i, cmd) in commands.iter().enumerate() {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            continue;
        }
        if started.elapsed() > timeout {
            bail!("Provision interrompue : délai global dépassé ({timeout:?}).");
        }
        eprintln!("  [{}/{}] {trimmed}", i + 1, commands.len());
        ssh_run_provision_command(private_key, user, host, port, trimmed, opts).with_context(
            || {
                format!(
                    "Provision échouée à l'étape {}/{}: {trimmed}",
                    i + 1,
                    commands.len()
                )
            },
        )?;
    }
    Ok(())
}

/// Monte des volumes virtiofs via SSH (arguments séparés, sans bash -c sur les chemins).
pub fn run_ssh_mount_steps(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    mounts: &[ResolvedMount],
    opts: &SshSessionOpts,
) -> Result<()> {
    for step in crate::mounts::ssh_mount_steps(mounts) {
        match step {
            MountSshStep::Mkdir { path } => {
                ssh_run_sudo_argv(
                    private_key,
                    user,
                    host,
                    port,
                    &["mkdir", "-p", "--", &path],
                    opts,
                )?;
            }
            MountSshStep::Mount { volume, path } => {
                if ssh_run_sudo_argv(
                    private_key,
                    user,
                    host,
                    port,
                    &["mount", "-t", "virtiofs", &volume, &path],
                    opts,
                )
                .is_err()
                {
                    // runcmd cloud-init peut avoir déjà monté le volume
                }
            }
            MountSshStep::Chown { path } => {
                ssh_run_sudo_argv(
                    private_key,
                    user,
                    host,
                    port,
                    &["chown", "-R", "virtum:virtum", &path],
                    opts,
                )?;
            }
        }
    }
    Ok(())
}

/// Quote pour `sh -c '…'` (argument unique passé à SSH).
fn shell_single_quote(script: &str) -> String {
    format!("'{}'", script.replace('\'', "'\"'\"'"))
}

/// Commande distante complète pour la provision (`sudo -n bash -c '…'` en un seul argument SSH).
fn remote_provision_shell_command(command: &str) -> String {
    let mut inner = command.trim();
    if let Some(rest) = inner.strip_prefix("sudo ") {
        inner = rest;
    }
    format!("sudo -n bash -c {}", shell_single_quote(inner))
}

/// Exécute une commande distante via `sudo -n bash -c` (utilisateur cloud-init = NOPASSWD).
pub fn ssh_run_provision_command(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    command: &str,
    opts: &SshSessionOpts,
) -> Result<()> {
    let remote = remote_provision_shell_command(command);
    ssh_run_remote_command(private_key, user, host, port, &remote, opts, true)
}

/// Exécute `sudo -n` + argv sur la VM (chemins en arguments séparés).
pub fn ssh_run_sudo_argv(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    argv: &[&str],
    opts: &SshSessionOpts,
) -> Result<()> {
    let mut remote_parts = vec!["sudo", "-n"];
    remote_parts.extend(argv);
    let remote = remote_parts.join(" ");
    ssh_run_remote_command(private_key, user, host, port, &remote, opts, true)
}

fn ssh_run_remote_command(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    remote: &str,
    opts: &SshSessionOpts,
    batch_mode: bool,
) -> Result<()> {
    let key = path_arg(private_key)?;
    let target = format!("{user}@{host}");
    let port_str = port.to_string();

    let mut cmd = Command::new("ssh");
    cmd.arg("-i").arg(key);
    append_security_opts(&mut cmd, opts, batch_mode)?;
    if port != 22 {
        cmd.arg("-p").arg(&port_str);
    }
    cmd.arg(&target).arg(remote);

    let output = cmd
        .output()
        .with_context(|| format!("Impossible d'exécuter ssh vers {target}"))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(msg) = map_ssh_host_key_error(&stderr, opts) {
        bail!("{msg}");
    }
    bail!(
        "Commande distante échouée (code {}): {stderr}",
        output.status.code().unwrap_or(-1)
    )
}

/// Commande SSH non interactive (BatchMode), ex. `bash -lc '…'`.
pub fn ssh_run_command(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    command: &str,
    opts: &SshSessionOpts,
) -> Result<()> {
    let key = path_arg(private_key)?;
    let target = format!("{user}@{host}");
    let port_str = port.to_string();

    let mut cmd = Command::new("ssh");
    cmd.arg("-i").arg(key);
    append_security_opts(&mut cmd, opts, true)?;
    if port != 22 {
        cmd.arg("-p").arg(&port_str);
    }
    cmd.arg(&target).arg("bash").arg("-lc").arg(command);

    let output = cmd
        .output()
        .with_context(|| format!("Impossible d'exécuter ssh vers {target}"))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(msg) = map_ssh_host_key_error(&stderr, opts) {
        bail!("{msg}");
    }
    bail!(
        "Commande distante échouée (code {}): {stderr}",
        output.status.code().unwrap_or(-1)
    )
}

/// Vérifie que l'authentification par clé Virtum fonctionne (cloud-init terminé).
pub fn ssh_auth_probe(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    opts: &SshSessionOpts,
) -> bool {
    let key = match path_arg(private_key) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let target = format!("{user}@{host}");
    let mut cmd = Command::new("ssh");
    cmd.arg("-i").arg(key);
    if append_security_opts(&mut cmd, opts, true).is_err() {
        return false;
    }
    if port != 22 {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(&target).arg("true");

    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn exec_ssh_shell(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    opts: &SshSessionOpts,
) -> Result<()> {
    let key = path_arg(private_key)?;
    let target = format!("{user}@{host}");

    let mut cmd = Command::new("ssh");
    cmd.arg("-i").arg(key);
    append_security_opts(&mut cmd, opts, false)?;
    if port != 22 {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg("-t")
        .arg(&target)
        .arg("bash")
        .arg("-l")
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

pub fn ssh_option_strings(opts: &SshSessionOpts) -> (String, String) {
    match opts.mode {
        SshSecurityMode::Insecure => (
            "StrictHostKeyChecking=no".into(),
            "UserKnownHostsFile=/dev/null".into(),
        ),
        SshSecurityMode::Secure => (
            "StrictHostKeyChecking=accept-new".into(),
            format!("UserKnownHostsFile={}", opts.known_hosts.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_provision_shell_command_quotes_and_sudo() {
        let cmd = remote_provision_shell_command(
            "DEBIAN_FRONTEND=noninteractive apt-get update -qq && apt-get install -y nginx",
        );
        assert!(cmd.starts_with("sudo -n bash -c '"));
        assert!(cmd.ends_with('\''));
        assert!(cmd.contains("apt-get update"));
        assert!(cmd.contains("&&"));
    }

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

    #[test]
    fn ssh_option_strings_secure_vs_insecure() {
        let dir = std::env::temp_dir().join(format!("virtum-ssh-opts-{}", std::process::id()));
        let opts = SshSessionOpts::new(&dir, false);
        let (strict, kh) = ssh_option_strings(&opts);
        assert_eq!(strict, "StrictHostKeyChecking=accept-new");
        assert!(kh.contains("known_hosts"));

        let opts_insec = SshSessionOpts::new(&dir, true);
        let (strict2, kh2) = ssh_option_strings(&opts_insec);
        assert_eq!(strict2, "StrictHostKeyChecking=no");
        assert_eq!(kh2, "UserKnownHostsFile=/dev/null");
    }
}
