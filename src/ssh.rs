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
use crate::metadata::{self, Instance, InstanceStatus};
use crate::mounts::{MountSshStep, ResolvedMount};
use crate::tr;
use crate::virtumfile::DEFAULT_SSH_PORT;

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
    /// Nom sous lequel la clé d'hôte est épinglée (`HostKeyAlias`) : l'id de la VM, pas
    /// son IP. Une autre VM qui obtient cette IP (bail usurpé, ARP, agent invité menteur)
    /// présente une autre clé et la connexion est refusée.
    pub host_key_alias: Option<String>,
}

impl SshSessionOpts {
    pub fn new(instance_dir: &Path, insecure_ssh: bool) -> Self {
        let host_key_alias = instance_dir
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| {
                !n.is_empty()
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
            .map(str::to_string);
        Self {
            mode: SshSecurityMode::from_flag(insecure_ssh),
            known_hosts: known_hosts_path(instance_dir),
            host_key_alias,
        }
    }
}

pub fn known_hosts_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("known_hosts")
}

/// Fichier temporaire de la clé d'hôte, supprimé dès qu'elle est lue (elle ne reste que
/// dans `user-data` et l'ISO cloud-init, lisibles par l'utilisateur et libvirt).
const GUEST_HOST_KEY_TMP: &str = "guest_host_key";

/// Clé d'hôte SSH d'une VM, générée par Virtum avant son premier démarrage.
#[derive(Debug, Clone)]
pub struct GuestHostKey {
    /// Clé privée OpenSSH (plusieurs lignes), remise à la VM par cloud-init.
    pub private_key: String,
    /// Clé publique `ssh-ed25519 <base64>`, sans commentaire.
    pub public_key: String,
}

/// Génère la clé d'hôte ed25519 d'une nouvelle VM.
///
/// La VM reçoit cette clé par cloud-init et Virtum l'épingle dans `known_hosts` avant la
/// première connexion : il n'y a plus de premier contact « accept-new » qu'une autre
/// machine du réseau pourrait intercepter en répondant à la place de la VM.
pub fn generate_guest_host_key(instance_dir: &Path) -> Result<GuestHostKey> {
    let private_path = instance_dir.join(GUEST_HOST_KEY_TMP);
    let public_path = instance_dir.join(format!("{GUEST_HOST_KEY_TMP}.pub"));
    for p in [&private_path, &public_path] {
        if fs::symlink_metadata(p).is_ok() {
            bail!(tr!(
                fr: "{} existe déjà : refus de générer la clé d'hôte par-dessus.",
                en: "{} already exists: refusing to generate the host key over it.",
                p.display()
            ));
        }
    }
    let result = (|| -> Result<GuestHostKey> {
        run_command(
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "",
                "-f",
                path_arg(&private_path)?,
            ],
        )
        .context(tr!(
            fr: "Impossible de générer la clé d'hôte SSH de la VM",
            en: "Cannot generate the VM's SSH host key"
        ))?;
        let private_key = fs::read_to_string(&private_path).with_context(|| {
            tr!(
                fr: "Impossible de lire {}",
                en: "Cannot read {}",
                private_path.display()
            )
        })?;
        let public_key =
            parse_ed25519_public_key(&fs::read_to_string(&public_path).with_context(|| {
                tr!(
                    fr: "Impossible de lire {}",
                    en: "Cannot read {}",
                    public_path.display()
                )
            })?)?;
        if !private_key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
            || private_key
                .lines()
                .any(|l| l.trim().is_empty() || l != l.trim())
        {
            bail!(tr!(
                fr: "Clé d'hôte SSH générée illisible.",
                en: "Generated SSH host key is unreadable."
            ));
        }
        Ok(GuestHostKey {
            private_key,
            public_key,
        })
    })();
    let _ = fs::remove_file(&private_path);
    let _ = fs::remove_file(&public_path);
    result
}

/// `ssh-ed25519 <base64>` sans commentaire ; refuse toute autre forme.
fn parse_ed25519_public_key(content: &str) -> Result<String> {
    let mut parts = content.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("ssh-ed25519"), Some(b64))
            if b64
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')) =>
        {
            Ok(format!("ssh-ed25519 {b64}"))
        }
        _ => bail!(tr!(
            fr: "Clé publique d'hôte SSH invalide.",
            en: "Invalid SSH host public key."
        )),
    }
}

/// Épingle la clé d'hôte de la VM dans le `known_hosts` de l'instance, sous son alias
/// (l'id de la VM), avant le premier démarrage.
pub fn pin_guest_host_key(instance_dir: &Path, key: &GuestHostKey) -> Result<()> {
    let opts = SshSessionOpts::new(instance_dir, false);
    let Some(alias) = opts.host_key_alias else {
        bail!(tr!(
            fr: "Nom d'instance inutilisable comme alias SSH : {}",
            en: "Instance name unusable as an SSH alias: {}",
            instance_dir.display()
        ));
    };
    let line = format!("{alias} {}\n", key.public_key);
    crate::fsutil::write_atomic(&opts.known_hosts, line.as_bytes(), 0o600)
}

/// `yes` quand la clé d'hôte est déjà épinglée (VM créée par Virtum ≥ 1.2, ou première
/// connexion déjà faite) ; `accept-new` seulement pour une VM plus ancienne jamais jointe.
fn strict_host_key_checking(opts: &SshSessionOpts) -> &'static str {
    let pinned = fs::metadata(&opts.known_hosts)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);
    if pinned {
        "StrictHostKeyChecking=yes"
    } else {
        "StrictHostKeyChecking=accept-new"
    }
}

/// Chemin de la clé privée Virtum (sans la créer).
pub fn private_key_path(keys_dir: &Path) -> PathBuf {
    keys_dir.join(PRIVATE_KEY_NAME)
}

#[derive(Debug, Clone)]
pub struct SshKeyPaths {
    pub private_key: PathBuf,
    pub public_key: PathBuf,
}

/// Creates the Virtum ed25519 key pair if missing; returns paths to private and public keys.
pub fn ensure_ssh_keypair(keys_dir: &Path) -> Result<SshKeyPaths> {
    fs::create_dir_all(keys_dir).with_context(
        || tr!(fr: "Impossible de créer {}", en: "Cannot create {}", keys_dir.display()),
    )?;
    restrict_keys_dir(keys_dir)?;

    let private_key = keys_dir.join(PRIVATE_KEY_NAME);
    let public_key = keys_dir.join(PUBLIC_KEY_NAME);

    if let Ok(meta) = fs::symlink_metadata(&private_key) {
        crate::fsutil::ensure_owned_regular_file(&private_key, &meta)
            .context(tr!(fr: "Clé privée Virtum refusée", en: "Virtum private key refused"))?;
        restrict_private_key_permissions(&private_key)?;
        // La clé publique injectée dans les VM est recalculée depuis la clé privée :
        // un .pub remplacé dans un dossier partagé ne peut pas autoriser une autre clé.
        sync_public_key_from_private(&private_key, &public_key)?;
        return Ok(SshKeyPaths {
            private_key,
            public_key,
        });
    }
    let _ = fs::remove_file(&public_key);

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
    let content = fs::read_to_string(public_key_path).with_context(|| {
        tr!(
            fr: "Impossible de lire {}",
            en: "Cannot read {}",
            public_key_path.display()
        )
    })?;
    let line = content.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        anyhow::bail!(tr!(
            fr: "Clé publique vide dans {}",
            en: "Empty public key in {}",
            public_key_path.display()
        ));
    }
    Ok(line.to_string())
}

fn warn_insecure_ssh_once() {
    if !INSECURE_SSH_WARNED.swap(true, Ordering::Relaxed) {
        crate::verr!(
            fr: "Avertissement : --insecure-ssh désactive la vérification de clé SSH \
             (StrictHostKeyChecking=no). À réserver aux scripts / CI de confiance. \
             OpenSSH peut afficher « Permanently added » (écriture dans /dev/null) : bruit sans effet.",
            en: "Warning: --insecure-ssh disables SSH host key checking \
             (StrictHostKeyChecking=no). Use it only for trusted scripts / CI. \
             OpenSSH may print 'Permanently added' (written to /dev/null): harmless noise."
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
            cmd.arg("-o").arg(strict_host_key_checking(opts));
            let kh = path_arg(&opts.known_hosts)?;
            cmd.arg("-o").arg(format!("UserKnownHostsFile={kh}"));
            cmd.arg("-o").arg("GlobalKnownHostsFile=/dev/null");
            if let Some(alias) = &opts.host_key_alias {
                cmd.arg("-o").arg(format!("HostKeyAlias={alias}"));
            }
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
        Some(tr!(
            fr: "Connexion refusée : la machine qui répond ne présente pas la clé d'hôte SSH \
             épinglée pour cette VM ({}). Une autre machine répond peut-être à sa place. \
             Si la VM a été réinstallée volontairement : virtum destroy <ref> puis recréez-la.",
            en: "Connection refused: the machine answering does not present the SSH host key \
             pinned for this VM ({}). Another machine may be answering in its place. \
             If the VM was reinstalled on purpose: run virtum destroy <ref>, then recreate it.",
            opts.known_hosts.display()
        ))
    } else {
        None
    }
}

/// Message pour une VM éteinte (cas typique : hôte redémarré, les VM ne repartent pas seules).
fn stopped_vm_message(instance: &Instance) -> String {
    let reference = instance.name.as_deref().unwrap_or(&instance.id);
    let relaunch = match instance.lab_name.as_deref() {
        Some(lab) => tr!(
            fr: "Relancez-la : `virtum start {reference}`, ou tout le lab '{lab}' : `virtum up` (dans le dossier du Virtumfile).",
            en: "Restart it: `virtum start {reference}`, or the whole lab '{lab}': `virtum up` (in the Virtumfile folder)."
        ),
        None => tr!(
            fr: "Relancez-la : `virtum start {reference}`.",
            en: "Restart it: `virtum start {reference}`."
        ),
    };
    tr!(
        fr: "La VM '{reference}' est éteinte (shut off) — l'hôte a peut-être redémarré. {relaunch}",
        en: "The VM '{reference}' is stopped (shut off) — the host may have rebooted. {relaunch}"
    )
}

/// SSH interactif vers une instance (étape 17) ; termine le processus avec le code SSH.
pub fn shell_to_instance(reference: &str, insecure_ssh: bool) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context(tr!(
            fr: "Impossible de démarrer le runtime Tokio",
            en: "Cannot start the Tokio runtime"
        ))?;

    let (private_key, user, host, port, ssh_opts) = runtime.block_on(async {
        let paths = config::ensure_layout().await?;
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        let instance = metadata::resolve_instance_ref(&metadata, reference)?.clone();
        let instance = crate::vm::refresh_instance_status(&paths.metadata, instance).await?;
        let vm_id = instance.id.as_str();

        if instance.status == InstanceStatus::Stopped {
            bail!("{}", stopped_vm_message(&instance));
        }
        if instance.status != InstanceStatus::Running {
            bail!(tr!(
                fr: "La VM '{vm_id}' n'est pas en cours d'exécution (statut: {:?}).",
                en: "The VM '{vm_id}' is not running (status: {:?}).",
                instance.status
            ));
        }

        let ip = instance
            .ip
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(tr!(
                    fr: "Aucune adresse IP enregistrée pour '{vm_id}'. \
                     Utilisez `virtum start` ou `virtum run`.",
                    en: "No IP address recorded for '{vm_id}'. \
                     Run `virtum start` or `virtum run`."
                ))
            })?;

        let configured = instance.ssh_port.unwrap_or(DEFAULT_SSH_PORT);
        // SLIRP (ancien mode) : `ssh_port` est le port hôte redirigé, pas de repli.
        let port = if ip == "127.0.0.1" || ip == SLIRP_GUEST_IPV4 {
            configured
        } else {
            reachable_ssh_port(ip, configured)
        };

        if (ip == "127.0.0.1" || ip == SLIRP_GUEST_IPV4) && instance.ssh_port.is_some() {
            crate::verr!(
                fr: "Avertissement: instance '{vm_id}' utilise l'ancien mode SLIRP (127.0.0.1:{port}). \
                 Relancez `virtum run` pour une IP dédiée (192.168.122.x).",
                en: "Warning: instance '{vm_id}' uses the legacy SLIRP mode (127.0.0.1:{port}). \
                 Run `virtum run` again for a dedicated IP (192.168.122.x)."
            );
        } else if ip == SLIRP_GUEST_IPV4 {
            bail!(tr!(
                fr: "La VM '{vm_id}' a l'IP SLIRP {SLIRP_GUEST_IPV4}, inaccessible depuis l'hôte. \
                 Recréez-la avec `virtum run` (réseau libvirt default).",
                en: "The VM '{vm_id}' has the SLIRP IP {SLIRP_GUEST_IPV4}, unreachable from the host. \
                 Recreate it with `virtum run` (libvirt default network)."
            ));
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
            bail!(tr!(
                fr: "Provision interrompue : délai global dépassé ({timeout:?}).",
                en: "Provisioning aborted: overall timeout exceeded ({timeout:?})."
            ));
        }
        crate::verr!("  [{}/{}] {trimmed}", i + 1, commands.len());
        ssh_run_provision_command(private_key, user, host, port, trimmed, opts).with_context(
            || {
                tr!(
                    fr: "Provision échouée à l'étape {}/{}: {trimmed}",
                    en: "Provisioning failed at step {}/{}: {trimmed}",
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
            MountSshStep::Mount {
                volume,
                path,
                read_only,
            } => {
                let mut argv = vec!["mount", "-t", "virtiofs"];
                if read_only {
                    argv.extend(["-o", "ro"]);
                }
                argv.extend([volume.as_str(), path.as_str()]);
                if ssh_run_sudo_argv(private_key, user, host, port, &argv, opts).is_err() {
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

    let output = cmd.output().with_context(
        || tr!(fr: "Impossible d'exécuter ssh vers {target}", en: "Cannot run ssh to {target}"),
    )?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stderr));
    if let Some(msg) = map_ssh_host_key_error(&stderr, opts) {
        bail!("{msg}");
    }
    bail!(tr!(
        fr: "Commande distante échouée (code {}): {stderr}",
        en: "Remote command failed (code {}): {stderr}",
        output.status.code().unwrap_or(-1)
    ))
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

    let output = cmd.output().with_context(
        || tr!(fr: "Impossible d'exécuter ssh vers {target}", en: "Cannot run ssh to {target}"),
    )?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stderr));
    if let Some(msg) = map_ssh_host_key_error(&stderr, opts) {
        bail!("{msg}");
    }
    bail!(tr!(
        fr: "Commande distante échouée (code {}): {stderr}",
        en: "Remote command failed (code {}): {stderr}",
        output.status.code().unwrap_or(-1)
    ))
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

/// Délai de connexion TCP pour choisir le port SSH joignable.
const TCP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ports SSH à essayer : le port déclaré, puis 22 (image fraîche, avant que la
/// configuration de la VM — Ansible, provision — ne déplace sshd).
pub fn ssh_ports_to_try(configured: u16) -> Vec<u16> {
    if configured == DEFAULT_SSH_PORT {
        vec![configured]
    } else {
        vec![configured, DEFAULT_SSH_PORT]
    }
}

/// Premier port de `ssh_ports_to_try` qui accepte une connexion TCP, sinon le port déclaré.
pub fn reachable_ssh_port(host: &str, configured: u16) -> u16 {
    ssh_ports_to_try(configured)
        .into_iter()
        .find(|port| tcp_port_open(host, *port))
        .unwrap_or(configured)
}

fn tcp_port_open(host: &str, port: u16) -> bool {
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::new(ip, port), TCP_PROBE_TIMEOUT)
        .is_ok()
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

    let status = cmd.status().with_context(
        || tr!(fr: "Impossible d'exécuter ssh vers {target}", en: "Cannot run ssh to {target}"),
    )?;

    std::process::exit(status.code().unwrap_or(1));
}

/// `keys/` réservé à l'utilisateur (0700) : QEMU n'a jamais besoin des clés.
fn restrict_keys_dir(keys_dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(keys_dir).with_context(
        || tr!(fr: "Impossible de lire {}", en: "Cannot read {}", keys_dir.display()),
    )?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        anyhow::bail!(tr!(
            fr: "{} doit être un répertoire (pas un lien).",
            en: "{} must be a directory (not a link).",
            keys_dir.display()
        ));
    }
    use std::os::unix::fs::MetadataExt;
    if meta.uid() == crate::fsutil::current_uid() && meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(keys_dir, Permissions::from_mode(0o700)).with_context(|| {
            tr!(
                fr: "Impossible de chmod 700 sur {}",
                en: "Cannot chmod 700 {}",
                keys_dir.display()
            )
        })?;
    }
    Ok(())
}

/// Réécrit la clé publique à partir de la clé privée (`ssh-keygen -y`) si elle diffère.
fn sync_public_key_from_private(private_key: &Path, public_key: &Path) -> Result<()> {
    let output = run_command(
        "ssh-keygen",
        &["-y", "-P", "", "-f", crate::command::path_arg(private_key)?],
    )
    .context(tr!(
        fr: "Impossible de dériver la clé publique Virtum (ssh-keygen -y)",
        en: "Cannot derive the Virtum public key (ssh-keygen -y)"
    ))?;
    let derived = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if derived.is_empty() {
        anyhow::bail!(tr!(
            fr: "ssh-keygen -y n'a renvoyé aucune clé publique.",
            en: "ssh-keygen -y returned no public key."
        ));
    }
    let wanted = format!("{derived} virtum\n");
    let current = fs::symlink_metadata(public_key)
        .ok()
        .filter(|m| m.is_file() && !m.file_type().is_symlink())
        .and_then(|_| fs::read_to_string(public_key).ok());
    let same_key = current
        .as_deref()
        .and_then(|c| c.lines().next())
        .is_some_and(|line| {
            line.split_whitespace()
                .take(2)
                .eq(derived.split_whitespace().take(2))
        });
    if !same_key {
        crate::fsutil::write_atomic(public_key, wanted.as_bytes(), 0o644)?;
    }
    Ok(())
}

fn restrict_private_key_permissions(private_key: &Path) -> Result<()> {
    fs::set_permissions(private_key, Permissions::from_mode(0o600)).with_context(|| {
        tr!(
            fr: "Impossible de définir les permissions 0600 sur {}",
            en: "Cannot set permissions 0600 on {}",
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
        SshSecurityMode::Secure => {
            let mut known = format!("UserKnownHostsFile={}", opts.known_hosts.display());
            if let Some(alias) = &opts.host_key_alias {
                known.push_str(&format!(" -o HostKeyAlias={alias}"));
            }
            (strict_host_key_checking(opts).into(), known)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped_instance(lab: Option<&str>) -> Instance {
        let mut instance: Instance = serde_json::from_str(
            r#"{"id":"vm-39c32f26cb1b","name":"app","os":"ubuntu-24.04","status":"stopped",
                "user":"virtum","hostname":"app","created_at":"2026-10-28T09:13:04Z"}"#,
        )
        .unwrap();
        instance.lab_name = lab.map(str::to_string);
        instance
    }

    #[test]
    fn ssh_ports_to_try_falls_back_to_22() {
        assert_eq!(ssh_ports_to_try(22), vec![22]);
        assert_eq!(ssh_ports_to_try(2222), vec![2222, 22]);
    }

    #[test]
    fn reachable_ssh_port_prefers_open_configured_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        assert_eq!(reachable_ssh_port("127.0.0.1", open), open);

        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        // Port déclaré fermé : repli sur 22 si un sshd local écoute, sinon port déclaré.
        assert!([closed, 22].contains(&reachable_ssh_port("127.0.0.1", closed)));
    }

    #[test]
    fn stopped_vm_message_points_to_lab_up() {
        let msg = stopped_vm_message(&stopped_instance(Some("infra-local")));
        assert!(msg.contains("'app' est éteinte"));
        assert!(msg.contains("virtum start app"));
        assert!(msg.contains("lab 'infra-local'"));
        assert!(msg.contains("virtum up"));
    }

    #[test]
    fn stopped_vm_message_standalone_vm() {
        let msg = stopped_vm_message(&stopped_instance(None));
        assert!(msg.contains("virtum start app"));
        assert!(!msg.contains("virtum up"));
    }

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
        // La clé est épinglée par VM (nom du dossier d'instance), pas par IP.
        let vm = SshSessionOpts::new(Path::new("/var/lib/virtum/instances/vm-0123abcd"), false);
        assert!(ssh_option_strings(&vm)
            .1
            .ends_with(" -o HostKeyAlias=vm-0123abcd"));

        let opts_insec = SshSessionOpts::new(&dir, true);
        let (strict2, kh2) = ssh_option_strings(&opts_insec);
        assert_eq!(strict2, "StrictHostKeyChecking=no");
        assert_eq!(kh2, "UserKnownHostsFile=/dev/null");
    }

    #[test]
    fn guest_host_key_is_generated_pinned_and_enforced() {
        if Command::new("ssh-keygen").arg("-?").output().is_err() {
            return;
        }
        let root = std::env::temp_dir().join(format!("virtum-hostkey-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("vm-0123abcd");
        fs::create_dir_all(&dir).unwrap();

        let key = generate_guest_host_key(&dir).unwrap();
        assert!(key.public_key.starts_with("ssh-ed25519 "));
        assert_eq!(key.public_key.split(' ').count(), 2);
        assert!(key.private_key.contains("OPENSSH PRIVATE KEY"));
        // Aucune copie de la clé privée ne reste dans le dossier de l'instance.
        assert!(!dir.join(GUEST_HOST_KEY_TMP).exists());
        assert!(!dir.join(format!("{GUEST_HOST_KEY_TMP}.pub")).exists());

        let opts = SshSessionOpts::new(&dir, false);
        // Avant épinglage (VM d'une ancienne version) : premier contact accept-new.
        assert_eq!(
            ssh_option_strings(&opts).0,
            "StrictHostKeyChecking=accept-new"
        );
        pin_guest_host_key(&dir, &key).unwrap();
        let pinned = fs::read_to_string(dir.join("known_hosts")).unwrap();
        assert_eq!(pinned, format!("vm-0123abcd {}\n", key.public_key));
        // Clé épinglée : plus aucune clé inconnue n'est acceptée.
        assert_eq!(ssh_option_strings(&opts).0, "StrictHostKeyChecking=yes");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn guest_host_key_public_key_is_validated() {
        assert_eq!(
            parse_ed25519_public_key("ssh-ed25519 AAAAC3Nz+/= comment\n").unwrap(),
            "ssh-ed25519 AAAAC3Nz+/="
        );
        assert!(parse_ed25519_public_key("ssh-rsa AAAA").is_err());
        assert!(parse_ed25519_public_key("ssh-ed25519 AA\"AA").is_err());
        assert!(parse_ed25519_public_key("").is_err());
    }
}
