//! Dossiers partagés hôte ↔ VM via virtiofs (Virtum v0.4).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::libvirt_access::{self, KVM_GROUP};
use crate::virtumfile::{MountSpec, Virtumfile};

pub const VIRTUM_GUEST_HOME: &str = "/home/virtum";

const MOUNTS_STATE_FILE: &str = "mounts-state.json";

/// Emplacements courants sur Debian/Ubuntu (le paquet n'ajoute pas toujours un lien dans `PATH`).
const VIRTIOFSD_CANDIDATES: &[&str] = &["/usr/libexec/virtiofsd", "/usr/lib/qemu/virtiofsd"];

/// Résout le binaire `virtiofsd` (`which` puis chemins FHS connus).
pub fn resolve_virtiofsd_path() -> Option<PathBuf> {
    if let Ok(output) = Command::new("which").arg("virtiofsd").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    VIRTIOFSD_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(|p| p.to_path_buf())
}

/// Mount résolu (chemins absolus hôte + invité).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedMount {
    pub name: String,
    /// Chemin relatif tel que dans le Virtumfile (ex. `./backend`).
    pub host: String,
    /// Chemin invité tel que dans le Virtumfile (ex. `~/app`).
    pub guest: String,
    pub host_abs: PathBuf,
    pub guest_path: String,
    pub socket_path: PathBuf,
    /// PID du virtiofsd lancé par Virtum (ne pas sonder le socket : une connexion le tue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtiofsd_pid: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum MountYamlEntry {
    Short(String),
    Detailed {
        name: String,
        host: String,
        guest: String,
    },
}

/// Parse une entrée YAML `mounts:` (liste hétérogène).
pub fn parse_mount_entries(raw: &[serde_yaml::Value]) -> Result<Vec<MountSpec>> {
    let mut out = Vec::new();
    for (i, value) in raw.iter().enumerate() {
        let entry: MountYamlEntry = serde_yaml::from_value(value.clone())
            .with_context(|| format!("mounts[{i}] : format invalide"))?;
        out.push(mount_entry_to_spec(entry, i)?);
    }
    Ok(out)
}

fn mount_entry_to_spec(entry: MountYamlEntry, index: usize) -> Result<MountSpec> {
    match entry {
        MountYamlEntry::Short(s) => {
            let (host, guest) = parse_mount_shortcut(&s)?;
            let name = default_volume_name(&host, index);
            Ok(MountSpec { name, host, guest })
        }
        MountYamlEntry::Detailed { name, host, guest } => Ok(MountSpec {
            name: validate_volume_name(&name)?,
            host: host.trim().to_string(),
            guest: guest.trim().to_string(),
        }),
    }
}

fn parse_mount_shortcut(s: &str) -> Result<(String, String)> {
    let s = s.trim();
    let (host, guest) = s
        .split_once(':')
        .with_context(|| format!("Mount invalide '{s}' (attendu host:guest, ex. ./app:~/app)"))?;
    Ok((host.trim().to_string(), guest.trim().to_string()))
}

fn default_volume_name(host: &str, index: usize) -> String {
    let base = Path::new(host)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("mount");
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        name = format!("mount-{index}");
    }
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        name = format!("m-{name}");
    }
    name
}

fn validate_volume_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("Le nom de volume mount ne peut pas être vide.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("Nom de volume '{name}' invalide (utilisez lettres, chiffres, '_' ou '-').");
    }
    Ok(name.to_string())
}

/// Fusionne lab.mounts + machine.mounts (machine écrase par `name`).
pub fn effective_mount_specs(lab: &Virtumfile, machine_name: &str) -> Vec<MountSpec> {
    let mut map: BTreeMap<String, MountSpec> = BTreeMap::new();
    if let Some(ref lab_mounts) = lab.mounts {
        for m in lab_mounts {
            map.insert(m.name.clone(), m.clone());
        }
    }
    if let Some(spec) = lab.machines.get(machine_name) {
        if let Some(ref machine_mounts) = spec.mounts {
            for m in machine_mounts {
                map.insert(m.name.clone(), m.clone());
            }
        }
    }
    map.into_values().collect()
}

/// Résout et valide les mounts d'une machine (chemins hôte sous Virtumfile, invité sous /home/virtum).
pub fn resolve_machine_mounts(
    lab: &Virtumfile,
    machine_name: &str,
    virtumfile_dir: &Path,
) -> Result<Vec<ResolvedMount>> {
    let specs = effective_mount_specs(lab, machine_name);
    let base = virtumfile_dir.canonicalize().with_context(|| {
        format!(
            "Répertoire Virtumfile invalide: {}",
            virtumfile_dir.display()
        )
    })?;
    let mut resolved = Vec::new();
    let mut names = std::collections::HashSet::new();
    for spec in specs {
        if !names.insert(spec.name.clone()) {
            bail!(
                "Machine '{machine_name}' : nom de volume mount '{}' en double.",
                spec.name
            );
        }
        resolved.push(resolve_mount_spec(&spec, &base)?);
    }
    Ok(resolved)
}

/// Résout un seul `MountSpec` (validation chemins).
pub fn resolve_mount_spec(spec: &MountSpec, virtumfile_base: &Path) -> Result<ResolvedMount> {
    let host_abs = resolve_host_path(virtumfile_base, &spec.host)?;
    let guest_path = resolve_guest_path(&spec.guest)?;
    Ok(ResolvedMount {
        name: spec.name.clone(),
        host: spec.host.clone(),
        guest: spec.guest.clone(),
        host_abs,
        guest_path,
        socket_path: PathBuf::new(), // rempli par ensure_virtiofs_servers
        virtiofsd_pid: None,
    })
}

pub fn resolve_host_path(virtumfile_base: &Path, host: &str) -> Result<PathBuf> {
    let host = host.trim();
    if host.is_empty() {
        bail!("Chemin hôte mount vide.");
    }
    if Path::new(host).is_absolute() {
        bail!(
            "Chemin hôte mount absolu interdit : {host}. \
             Utilisez un chemin relatif au Virtumfile (ex. ./src)."
        );
    }
    let joined = virtumfile_base.join(host);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                bail!("Chemin hôte '{host}' sort du répertoire du Virtumfile (.. interdit).")
            }
            Component::CurDir => {}
            Component::Normal(c) => normalized.push(c),
            Component::RootDir => normalized.push(component),
            Component::Prefix(p) => normalized.push(Component::Prefix(p)),
        }
    }
    let base = virtumfile_base
        .canonicalize()
        .context("canonicalize virtumfile base")?;
    if !host_path_under_virtumfile_root(&normalized, &base) {
        bail!(
            "Chemin hôte '{}' résout hors du répertoire Virtumfile ({})",
            host,
            base.display()
        );
    }
    if normalized.exists() {
        if !normalized.is_dir() {
            bail!(
                "Le chemin hôte mount doit être un répertoire : {}",
                normalized.display()
            );
        }
    } else {
        fs::create_dir_all(&normalized).with_context(|| {
            format!(
                "Impossible de créer le répertoire hôte mount {} (résolu depuis '{host}').",
                normalized.display()
            )
        })?;
    }
    let canonical = normalized
        .canonicalize()
        .with_context(|| format!("Impossible de résoudre {}", normalized.display()))?;
    if !canonical.starts_with(&base) {
        bail!(
            "Chemin hôte '{}' résout hors du répertoire Virtumfile ({})",
            host,
            base.display()
        );
    }
    Ok(canonical)
}

/// Vérifie que `candidate` reste sous `base` (composants de chemin, avant `canonicalize`).
fn host_path_under_virtumfile_root(candidate: &Path, base: &Path) -> bool {
    let mut cand = candidate.components();
    for b in base.components() {
        match (cand.next(), Some(b)) {
            (Some(c), Some(b)) if c == b => {}
            _ => return false,
        }
    }
    true
}

pub fn resolve_guest_path(guest: &str) -> Result<String> {
    let guest = guest.trim();
    if guest.is_empty() {
        bail!("Chemin invité mount vide.");
    }
    let path = if let Some(rest) = guest.strip_prefix("~/") {
        format!("{VIRTUM_GUEST_HOME}/{rest}")
    } else if guest == "~" {
        VIRTUM_GUEST_HOME.to_string()
    } else if guest.starts_with(VIRTUM_GUEST_HOME) {
        guest.to_string()
    } else if guest.starts_with('/') {
        bail!("Chemin invité '{guest}' interdit : doit être sous {VIRTUM_GUEST_HOME} (ex. ~/app).");
    } else {
        bail!(
            "Chemin invité '{guest}' invalide : utilisez ~/chemin ou {VIRTUM_GUEST_HOME}/chemin."
        );
    };
    if path.contains("..") {
        bail!("Chemin invité '{guest}' ne doit pas contenir '..'.");
    }
    Ok(path)
}

pub fn socket_path_for(instance_dir: &Path, name: &str) -> PathBuf {
    instance_dir.join(format!("virtiofs-{name}.sock"))
}

fn path_is_socket(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// virtiofsd tourne encore (PID enregistré + socket présent).
/// Ne jamais se connecter au socket : virtiofsd s'arrête à la déconnexion du client.
fn virtiofs_daemon_alive(socket_path: &Path, pid: Option<u32>) -> bool {
    path_is_socket(socket_path) && pid.is_some_and(daemon_running)
}

/// Démarre virtiofsd pour chaque mount ; remplit `socket_path` et écrit `mounts-state.json`.
pub fn ensure_virtiofs_servers(instance_dir: &Path, mounts: &mut [ResolvedMount]) -> Result<()> {
    libvirt_access::prepare_instance_dir_for_virtiofs(instance_dir)?;
    for mount in mounts.iter_mut() {
        mount.socket_path = socket_path_for(instance_dir, &mount.name);
        ensure_virtiofs_socket_running(instance_dir, mount)?;
    }
    save_mounts_state(instance_dir, mounts)?;
    Ok(())
}

fn ensure_virtiofs_socket_running(instance_dir: &Path, mount: &mut ResolvedMount) -> Result<()> {
    if virtiofs_daemon_alive(&mount.socket_path, mount.virtiofsd_pid) {
        libvirt_access::prepare_virtiofs_socket(&mount.socket_path)?;
        return Ok(());
    }
    if let Some(pid) = mount.virtiofsd_pid.take() {
        let _ = stop_virtiofsd_pid(pid);
    }
    if mount.socket_path.exists() {
        std::fs::remove_file(&mount.socket_path).with_context(|| {
            format!(
                "Socket virtiofs orphelin (démon arrêté) : {}",
                mount.socket_path.display()
            )
        })?;
    }
    mount.virtiofsd_pid = Some(start_virtiofsd(
        instance_dir,
        &mount.name,
        &mount.host_abs,
        &mount.socket_path,
    )?);
    Ok(())
}

fn start_virtiofsd(
    instance_dir: &Path,
    mount_name: &str,
    shared_dir: &Path,
    socket_path: &Path,
) -> Result<u32> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Impossible de créer {}", parent.display()))?;
    }
    let shared = shared_dir
        .canonicalize()
        .with_context(|| format!("Répertoire partagé {}", shared_dir.display()))?;
    let socket = socket_path.to_path_buf();

    let virtiofsd = resolve_virtiofsd_path().with_context(|| {
        "virtiofsd introuvable. Installez le paquet virtiofsd (sudo apt install virtiofsd) \
         puis relancez `virtum doctor`."
    })?;

    let log_path = instance_dir.join(format!("virtiofs-{mount_name}.log"));
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Impossible d'ouvrir {}", log_path.display()))?;

    // Sans --sandbox=none, virtiofsd peut quitter si uidmap/newuidmap est absent.
    let mut cmd = Command::new(&virtiofsd);
    cmd.args([
        "--sandbox=none",
        "--socket-path",
        socket.to_str().context("socket path non UTF-8")?,
        "--shared-dir",
        shared.to_str().context("shared-dir non UTF-8")?,
        "--socket-group",
        KVM_GROUP,
        "--cache=auto",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::from(log_file));
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| {
        format!(
            "Impossible de démarrer virtiofsd pour {} → {}. \
                 Lancez `virtum doctor` (paquet virtiofsd).",
            shared.display(),
            socket.display()
        )
    })?;

    let daemon_pid = child.id();

    // Attendre que le socket soit prêt et que le démon reste vivant
    for _ in 0..50 {
        if !daemon_running(daemon_pid) {
            let log_hint = instance_dir.join(format!("virtiofs-{mount_name}.log"));
            bail!(
                "virtiofsd s'est arrêté tout de suite pour le volume '{mount_name}'. \
                 Consultez {} (souvent sandbox/uidmap ; Virtum utilise --sandbox=none).",
                log_hint.display()
            );
        }
        if path_is_socket(&socket) && daemon_running(daemon_pid) {
            libvirt_access::prepare_virtiofs_socket(&socket)?;
            return Ok(daemon_pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let log_hint = instance_dir.join(format!("virtiofs-{mount_name}.log"));
    bail!(
        "Timeout en attendant le socket virtiofsd {}. \
         Consultez {} et `ps aux | grep virtiofsd`.",
        socket.display(),
        log_hint.display()
    );
}

fn daemon_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn load_mounts_state(instance_dir: &Path) -> Result<Vec<ResolvedMount>> {
    let path = instance_dir.join(MOUNTS_STATE_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("{} invalide", path.display()))
}

fn save_mounts_state(instance_dir: &Path, mounts: &[ResolvedMount]) -> Result<()> {
    let path = instance_dir.join(MOUNTS_STATE_FILE);
    let content = serde_json::to_string_pretty(mounts).context("sérialisation mounts-state")?;
    std::fs::write(&path, format!("{content}\n"))
        .with_context(|| format!("Impossible d'écrire {}", path.display()))?;
    Ok(())
}

fn stop_virtiofsd_pid(pid: u32) -> bool {
    Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Chemins hôte absolus des mounts d'une instance (`mounts-state.json`).
pub fn host_dirs_from_instance(instance_dir: &Path) -> Vec<PathBuf> {
    load_mounts_state(instance_dir)
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.host_abs)
        .collect()
}

/// Union des répertoires hôte `mounts:` pour toutes les machines du lab.
pub fn collect_host_dirs_for_lab(lab: &Virtumfile, virtumfile_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut seen = BTreeMap::<PathBuf, ()>::new();
    for machine_name in lab.machines.keys() {
        for mount in resolve_machine_mounts(lab, machine_name, virtumfile_dir)? {
            seen.insert(mount.host_abs, ());
        }
    }
    Ok(seen.into_keys().collect())
}

/// Plus long préfixe commun des chemins mount (répertoire du Virtumfile attendu).
pub fn infer_mount_purge_root(dirs: &[PathBuf]) -> Result<PathBuf> {
    let first = dirs
        .first()
        .context("--purge : aucun dossier mount enregistré pour cette VM")?;
    let mut root = if first.is_dir() {
        first.clone()
    } else {
        first
            .parent()
            .context("Chemin mount sans répertoire parent")?
            .to_path_buf()
    };
    for dir in dirs.iter().skip(1) {
        let d = if dir.is_dir() {
            dir.clone()
        } else {
            dir.parent()
                .context("Chemin mount sans répertoire parent")?
                .to_path_buf()
        };
        while !d.starts_with(&root) {
            root = root
                .parent()
                .context("Impossible de déterminer la racine des mounts")?
                .to_path_buf();
        }
    }
    Ok(root)
}

/// Supprime les répertoires hôte déclarés dans `mounts:` (option `--purge` au destroy).
/// Ne supprime que des sous-répertoires stricts de `allowed_root` (répertoire du Virtumfile).
pub fn purge_host_mount_dirs(dirs: &[PathBuf], allowed_root: &Path) -> Result<usize> {
    let base = allowed_root
        .canonicalize()
        .with_context(|| format!("Répertoire Virtumfile invalide: {}", allowed_root.display()))?;
    let mut purged = 0usize;
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let canonical = dir
            .canonicalize()
            .with_context(|| format!("Impossible de résoudre {}", dir.display()))?;
        if canonical == base {
            bail!(
                "Refus de supprimer le répertoire du Virtumfile lui-même ({})",
                base.display()
            );
        }
        if !canonical.starts_with(&base) {
            bail!(
                "Refus de supprimer {} : hors du répertoire Virtumfile ({})",
                canonical.display(),
                base.display()
            );
        }
        if !canonical.is_dir() {
            continue;
        }
        std::fs::remove_dir_all(&canonical).with_context(|| {
            format!(
                "Impossible de supprimer le dossier mount hôte {}",
                canonical.display()
            )
        })?;
        eprintln!("Dossier mount supprimé : {}", canonical.display());
        purged += 1;
    }
    Ok(purged)
}

/// Arrête les virtiofsd et supprime sockets / état (destroy / cleanup).
pub fn teardown_virtiofs(instance_dir: &Path) {
    let mounts = load_mounts_state(instance_dir).unwrap_or_default();
    for mount in &mounts {
        if let Some(pid) = mount.virtiofsd_pid {
            let _ = stop_virtiofsd_pid(pid);
        }
        let _ = std::fs::remove_file(&mount.socket_path);
    }
    let state = instance_dir.join(MOUNTS_STATE_FILE);
    let _ = std::fs::remove_file(state);
    // Les processus virtiofsd se terminent souvent quand le socket est supprimé ;
    // sinon l'utilisateur peut les arrêter manuellement.
    let _ = Command::new("pkill")
        .args(["-f"])
        .arg(format!("virtiofsd.*{}", instance_dir.display()))
        .status();
}

pub fn mounts_to_instance_specs(mounts: &[ResolvedMount]) -> Vec<MountSpec> {
    mounts
        .iter()
        .map(|m| MountSpec {
            name: m.name.clone(),
            host: m.host.clone(),
            guest: m.guest.clone(),
        })
        .collect()
}

/// Nouveaux mounts présents dans desired mais pas dans stored (par `name`).
pub fn mounts_to_add<'a>(desired: &'a [MountSpec], stored: &[MountSpec]) -> Vec<&'a MountSpec> {
    let stored_names: std::collections::HashSet<_> = stored.iter().map(|m| &m.name).collect();
    desired
        .iter()
        .filter(|m| !stored_names.contains(&m.name))
        .collect()
}

/// Noms retirés du Virtumfile mais encore en metadata.
pub fn mounts_removed_from_file(stored: &[MountSpec], desired: &[MountSpec]) -> Vec<String> {
    let desired_names: std::collections::HashSet<_> =
        desired.iter().map(|m| m.name.as_str()).collect();
    stored
        .iter()
        .filter(|m| !desired_names.contains(m.name.as_str()))
        .map(|m| m.name.clone())
        .collect()
}

/// Changement host/guest sur un volume existant.
pub fn mounts_changed_blocking(stored: &[MountSpec], desired: &[MountSpec]) -> Vec<String> {
    let mut errors = Vec::new();
    let stored_map: BTreeMap<_, _> = stored.iter().map(|m| (&m.name, m)).collect();
    for d in desired {
        if let Some(s) = stored_map.get(&d.name) {
            if s.host != d.host || s.guest != d.guest {
                errors.push(format!(
                    "volume '{}' : chemins modifiés ({}:{} → {}:{}). \
                     Utilisez virtum destroy puis virtum up.",
                    d.name, s.host, s.guest, d.host, d.guest
                ));
            }
        }
    }
    errors
}

/// Commandes shell pour monter des volumes après boot (apply / provision).
pub fn ssh_mount_commands(mounts: &[ResolvedMount]) -> Vec<String> {
    let mut cmds = Vec::new();
    for m in mounts {
        cmds.push(format!("mkdir -p {}", m.guest_path));
        cmds.push(format!(
            "mount -t virtiofs {} {} || true",
            m.name, m.guest_path
        ));
        cmds.push(format!("chown -R virtum:virtum {}", m.guest_path));
    }
    cmds
}

pub fn format_mount_list(mounts: &[MountSpec]) -> String {
    if mounts.is_empty() {
        return "-".to_string();
    }
    mounts
        .iter()
        .map(|m| format!("{}:{}→{}", m.name, m.host, m.guest))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_guest_path_tilde() {
        assert_eq!(resolve_guest_path("~/app").unwrap(), "/home/virtum/app");
        assert_eq!(resolve_guest_path("~").unwrap(), "/home/virtum");
        assert_eq!(
            resolve_guest_path("/home/virtum/data").unwrap(),
            "/home/virtum/data"
        );
        assert!(resolve_guest_path("/etc/foo").is_err());
    }

    #[test]
    fn resolve_host_rejects_parent_dir() {
        let dir = std::env::temp_dir().join(format!("virtum-mount-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let sub = dir.join("proj");
        fs::create_dir_all(&sub).unwrap();
        assert!(resolve_host_path(&dir, "../outside").is_err());
        assert!(resolve_host_path(&dir, "./proj").is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_host_creates_missing_dir() {
        let dir = std::env::temp_dir().join(format!("virtum-mount-create-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("auto-created");
        assert!(!missing.exists());
        let resolved = resolve_host_path(&dir, "./auto-created").unwrap();
        assert!(missing.is_dir());
        assert_eq!(resolved, missing.canonicalize().unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_virtiofsd_finds_libexec_on_debian() {
        let path = resolve_virtiofsd_path();
        if std::path::Path::new("/usr/libexec/virtiofsd").is_file() {
            assert_eq!(path.as_deref(), Some(Path::new("/usr/libexec/virtiofsd")));
        }
    }

    #[test]
    fn infer_mount_purge_root_common_parent() {
        let base = std::env::temp_dir().join(format!("virtum-purge-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("common")).unwrap();
        fs::create_dir_all(base.join("backend")).unwrap();
        let dirs = vec![base.join("common"), base.join("backend")];
        let root = infer_mount_purge_root(&dirs).unwrap();
        assert_eq!(root, base);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn parse_mount_shortcut_entry() {
        let spec = mount_entry_to_spec(MountYamlEntry::Short("./backend:~/app".into()), 0).unwrap();
        assert_eq!(spec.host, "./backend");
        assert_eq!(spec.guest, "~/app");
        assert_eq!(spec.name, "backend");
    }
}
