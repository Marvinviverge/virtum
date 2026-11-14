//! Dossiers partagés hôte ↔ VM via virtiofs (Virtum v0.4).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::libvirt_access;
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
    /// Répertoire (canonique) du Virtumfile à la création : racine autorisée pour ce
    /// partage (relance de virtiofsd, `destroy --purge`). Absent dans un état antérieur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtumfile_dir: Option<PathBuf>,
    /// virtiofsd sert le partage en lecture seule (imposé côté hôte). Absent d'un état
    /// antérieur à la v0.10 : le démon qui tourne a été lancé en écriture.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum MountYamlEntry {
    Short(String),
    Detailed {
        name: String,
        host: String,
        guest: String,
        #[serde(default)]
        rw: bool,
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
            let (host, guest, rw) = parse_mount_shortcut(&s)?;
            let name = default_volume_name(&host, index);
            Ok(MountSpec {
                name,
                host,
                guest,
                rw,
            })
        }
        MountYamlEntry::Detailed {
            name,
            host,
            guest,
            rw,
        } => Ok(MountSpec {
            name: validate_volume_name(&name)?,
            host: host.trim().to_string(),
            guest: guest.trim().to_string(),
            rw,
        }),
    }
}

/// `host:guest[:ro|:rw]` ; lecture seule par défaut.
fn parse_mount_shortcut(s: &str) -> Result<(String, String, bool)> {
    let s = s.trim();
    let (host, rest) = s.split_once(':').with_context(|| {
        format!("Mount invalide '{s}' (attendu host:guest[:rw], ex. ./app:~/app:rw)")
    })?;
    let (guest, rw) = match rest.rsplit_once(':') {
        Some((guest, "rw")) => (guest, true),
        Some((guest, "ro")) => (guest, false),
        _ => (rest, false),
    };
    Ok((host.trim().to_string(), guest.trim().to_string(), rw))
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
    let pairs: Vec<_> = resolved
        .iter()
        .map(|m| (format!("{machine_name}/{}", m.name), m.host_abs.clone()))
        .collect();
    ensure_mounts_not_nested(&pairs)?;
    Ok(resolved)
}

/// Refuse un partage dont le répertoire hôte est strictement inclus dans celui d'un autre.
///
/// Un invité qui écrit dans le partage englobant pourrait remplacer le répertoire du
/// partage inclus par un lien symbolique vers n'importe quel dossier de l'hôte. Le même
/// répertoire partagé par plusieurs machines (mounts du lab) reste autorisé.
pub fn ensure_mounts_not_nested(mounts: &[(String, PathBuf)]) -> Result<()> {
    for (outer_label, outer) in mounts {
        for (inner_label, inner) in mounts {
            if inner != outer && inner.starts_with(outer) {
                bail!(
                    "Mounts imbriqués interdits : '{inner_label}' ({}) est à l'intérieur de \
                     '{outer_label}' ({}). Un invité pourrait remplacer le dossier inclus par un \
                     lien symbolique vers un autre dossier de l'hôte ; utilisez des dossiers disjoints.",
                    inner.display(),
                    outer.display()
                );
            }
        }
    }
    Ok(())
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
        virtumfile_dir: Some(virtumfile_base.to_path_buf()),
        read_only: !spec.rw,
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
    let base = virtumfile_base
        .canonicalize()
        .context("canonicalize virtumfile base")?;

    // Composants relatifs, sans `.` ; `..` interdit.
    let mut rel = Vec::new();
    for component in Path::new(host).components() {
        match component {
            Component::ParentDir => {
                bail!("Chemin hôte '{host}' sort du répertoire du Virtumfile (.. interdit).")
            }
            Component::CurDir => {}
            Component::Normal(c) => rel.push(c.to_os_string()),
            Component::RootDir | Component::Prefix(_) => {
                bail!("Chemin hôte mount absolu interdit : {host}.")
            }
        }
    }

    // Parcours composant par composant depuis la base, sans suivre de lien symbolique :
    // rien n'est créé tant que le chemin déjà parcouru n'est pas vérifié. Un lien
    // symbolique (dépôt cloné, ou planté par un invité dans un partage) est refusé.
    let mut current = base.clone();
    for c in &rel {
        current.push(c);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => bail!(
                "Chemin hôte '{host}' : {} est un lien symbolique (refusé ; utilisez le \
                 répertoire réel sous le Virtumfile).",
                current.display()
            ),
            Ok(meta) if !meta.is_dir() => bail!(
                "Le chemin hôte mount doit être un répertoire : {}",
                current.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!(
                        "Impossible de créer le répertoire hôte mount {} (résolu depuis '{host}').",
                        current.display()
                    )
                })?;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Impossible de lire {}", current.display()))
            }
        }
    }

    let canonical = current
        .canonicalize()
        .with_context(|| format!("Impossible de résoudre {}", current.display()))?;
    if canonical != current || !host_path_under_virtumfile_root(&canonical, &base) {
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
    let raw = guest.trim();
    if raw.is_empty() {
        bail!(guest_path_error(raw, "chemin vide"));
    }

    let normalized = normalize_guest_path(raw)?;
    if normalized.contains("..") {
        bail!(guest_path_error(raw, "ne doit pas contenir '..'"));
    }
    validate_guest_path_chars(raw, &normalized)?;
    if !is_strict_virtum_home(&normalized) {
        bail!(guest_path_error(
            raw,
            "doit être sous /home/virtum (ex. ~/app)"
        ));
    }
    Ok(normalized)
}

fn guest_path_error(raw: &str, reason: &str) -> String {
    format!("Chemin invité '{raw}' invalide : {reason}")
}

fn normalize_guest_path(raw: &str) -> Result<String> {
    if raw == "~" {
        return Ok(VIRTUM_GUEST_HOME.to_string());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if rest.is_empty() {
            return Ok(VIRTUM_GUEST_HOME.to_string());
        }
        return Ok(format!("{VIRTUM_GUEST_HOME}/{rest}"));
    }
    if raw.starts_with('/') {
        return Ok(raw.to_string());
    }
    bail!(guest_path_error(
        raw,
        "utilisez ~/chemin ou /home/virtum/chemin"
    ))
}

/// Préfixe strict `/home/virtum` ou `/home/virtum/…` (pas `/home/virtum-evil`).
fn is_strict_virtum_home(path: &str) -> bool {
    if path == VIRTUM_GUEST_HOME {
        return true;
    }
    path.starts_with(&format!("{VIRTUM_GUEST_HOME}/"))
}

fn validate_guest_path_chars(raw: &str, normalized: &str) -> Result<()> {
    for c in normalized.chars() {
        if c.is_ascii_alphanumeric() || c == '/' || c == '.' || c == '_' || c == '-' {
            continue;
        }
        bail!(guest_path_error(
            raw,
            &format!("caractère '{c}' interdit (autorisés : [a-zA-Z0-9/_.-])")
        ));
    }
    Ok(())
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
    path_is_socket(socket_path) && pid.is_some_and(|p| is_our_virtiofsd(p, socket_path))
}

/// Le PID est bien un virtiofsd de l'utilisateur servant ce socket (PID réutilisé ou
/// état altéré : on ne le considère pas comme le nôtre).
fn is_our_virtiofsd(pid: u32, socket_path: &Path) -> bool {
    crate::procs::process_matches(pid, "virtiofsd", &socket_path.to_string_lossy())
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
        stop_virtiofsd_pid(pid, &mount.socket_path);
    }
    if mount.socket_path.exists() {
        std::fs::remove_file(&mount.socket_path).with_context(|| {
            format!(
                "Socket virtiofs orphelin (démon arrêté) : {}",
                mount.socket_path.display()
            )
        })?;
    }
    validate_stored_share(mount)?;
    restrict_share_root(&mount.name, &mount.host_abs)?;
    mount.virtiofsd_pid = Some(start_virtiofsd(
        instance_dir,
        &mount.name,
        &mount.host_abs,
        &mount.socket_path,
        mount.read_only,
    )?);
    Ok(())
}

/// Ferme la racine d'un partage à tout autre compte local avant de lancer virtiofsd.
///
/// virtiofsd s'exécute sous l'utilisateur Virtum : root dans l'invité peut créer dans le
/// partage un exécutable setuid/setgid appartenant à cet utilisateur (un `chmod 6755` du
/// propriétaire n'est filtré ni par virtiofsd ni par le noyau ; `--killpriv-v2` n'efface ces
/// bits qu'à l'écriture). Les partages imbriqués étant refusés, la racine est le seul chemin
/// d'accès ; elle ne doit être traversable par personne d'autre :
/// - entrée ACL nommée donnant `x` à un autre compte ou groupe : partage refusé (choix
///   délibéré de l'utilisateur, on ne l'efface pas) ;
/// - accès « autres » : retiré ;
/// - accès groupe : retiré si le groupe du dossier contient un autre compte.
fn restrict_share_root(name: &str, shared: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let uid = crate::fsutil::current_uid();
    let meta = fs::symlink_metadata(shared)
        .with_context(|| format!("Répertoire partagé {} introuvable", shared.display()))?;

    let acl = crate::share_access::named_acl_traversers_of(shared, uid)?;
    if !acl.is_empty() {
        bail!(
            "Volume '{name}' : une ACL sur {} permet à d'autres comptes de traverser le partage \
             ({}). Un invité pourrait y déposer un exécutable setuid qu'ils pourraient lancer. \
             Retirez ces entrées (setfacl -x …) puis relancez.",
            shared.display(),
            acl.join(", ")
        );
    }

    let mode = meta.permissions().mode() & 0o7777;
    let shared_group = crate::share_access::group_has_other_members(meta.gid(), uid);
    let mut remove = 0o007;
    if shared_group {
        remove |= 0o070;
    }
    if mode & remove == 0 {
        return Ok(());
    }
    let new_mode = mode & !remove;
    if meta.uid() != uid {
        bail!(
            "Volume '{name}' : {} est accessible à d'autres comptes (mode {mode:o}) et \
             n'appartient pas à l'utilisateur courant. Un invité pourrait y déposer un \
             exécutable setuid atteignable par ces comptes : passez-le en {new_mode:o} puis \
             relancez.",
            shared.display()
        );
    }
    fs::set_permissions(shared, fs::Permissions::from_mode(new_mode))
        .with_context(|| format!("Impossible de restreindre l'accès à {}", shared.display()))?;
    let who = if shared_group {
        "« autres » et groupe partagé"
    } else {
        "« autres »"
    };
    crate::verr!(
        "Volume '{name}' : accès {who} retiré sur {} ({mode:o} → {new_mode:o}), pour qu'aucun \
         autre compte local n'atteigne les fichiers déposés par l'invité.",
        shared.display()
    );
    Ok(())
}

/// Revalide le répertoire partagé juste avant de (re)lancer virtiofsd.
///
/// `host_abs` a été validé à la création, mais un invité qui écrit dans un autre partage,
/// ou un état altéré, peut l'avoir redirigé : il doit encore être un répertoire réel,
/// canonique (aucun composant devenu lien symbolique) et sous le répertoire du Virtumfile.
fn validate_stored_share(mount: &ResolvedMount) -> Result<()> {
    let shared = &mount.host_abs;
    if !shared.is_absolute() {
        bail!(
            "Volume '{}' : chemin hôte enregistré non absolu ({}).",
            mount.name,
            shared.display()
        );
    }
    let meta = fs::symlink_metadata(shared)
        .with_context(|| format!("Répertoire partagé {} introuvable", shared.display()))?;
    let canonical = shared
        .canonicalize()
        .with_context(|| format!("Répertoire partagé {}", shared.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() || canonical != *shared {
        bail!(
            "Volume '{}' : {} a été remplacé (lien symbolique ou autre chemin réel {}). \
             Partage refusé ; vérifiez le dossier puis recréez la VM.",
            mount.name,
            shared.display(),
            canonical.display()
        );
    }
    if let Some(root) = &mount.virtumfile_dir {
        if !host_path_under_virtumfile_root(&canonical, root) {
            bail!(
                "Volume '{}' : {} est hors du répertoire Virtumfile ({}). Partage refusé.",
                mount.name,
                canonical.display(),
                root.display()
            );
        }
    }
    Ok(())
}

/// `virtiofsd --readonly` existe (virtiofsd ≥ 1.11).
fn virtiofsd_supports_readonly(virtiofsd: &Path) -> bool {
    Command::new(virtiofsd)
        .arg("--help")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).contains("--readonly")
                || String::from_utf8_lossy(&o.stderr).contains("--readonly")
        })
        .unwrap_or(false)
}

/// bubblewrap, pour servir un partage en lecture seule avec un virtiofsd sans `--readonly`.
pub fn resolve_bwrap_path() -> Option<PathBuf> {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Comment imposer la lecture seule côté hôte : option native ou bubblewrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyBackend {
    Native,
    Bwrap(PathBuf),
}

/// Moyen d'imposer la lecture seule avec ce virtiofsd, `None` s'il n'y en a pas.
pub fn read_only_backend(virtiofsd: &Path) -> Option<ReadOnlyBackend> {
    if virtiofsd_supports_readonly(virtiofsd) {
        return Some(ReadOnlyBackend::Native);
    }
    resolve_bwrap_path().map(ReadOnlyBackend::Bwrap)
}

fn start_virtiofsd(
    instance_dir: &Path,
    mount_name: &str,
    shared_dir: &Path,
    socket_path: &Path,
    read_only: bool,
) -> Result<u32> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Impossible de créer {}", parent.display()))?;
    }
    let shared = shared_dir.to_path_buf();
    let socket = socket_path.to_path_buf();

    let virtiofsd = resolve_virtiofsd_path().with_context(|| {
        "virtiofsd introuvable. Installez le paquet virtiofsd (sudo apt install virtiofsd) \
         puis relancez `virtum doctor`."
    })?;

    let log_path = instance_dir.join(format!("virtiofs-{mount_name}.log"));
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log_path)
        .with_context(|| format!("Impossible d'ouvrir {}", log_path.display()))?;

    // Sans --sandbox=none, virtiofsd peut quitter si uidmap/newuidmap est absent.
    // --killpriv-v2 : toute écriture, chown ou troncature par l'invité efface les bits
    // setuid/setgid (et capacités) des fichiers hôte. --socket-group seulement en repli
    // sans setfacl (sinon socket 0600 + ACL libvirt-qemu).
    //
    // Lecture seule : imposée côté hôte (l'invité est root chez lui, un `mount -o ro` dans
    // la VM ne protège rien). virtiofsd ≥ 1.11 a `--readonly` ; sinon il tourne sous
    // bubblewrap, dans un espace de noms où le dossier partagé est monté en lecture seule.
    let backend = if read_only {
        Some(read_only_backend(&virtiofsd).with_context(|| {
            format!(
                "Volume '{mount_name}' en lecture seule : ce virtiofsd n'a pas `--readonly` et \
                 bubblewrap est absent. Installez-le (sudo apt install bubblewrap) ou déclarez \
                 le partage `rw: true`."
            )
        })?)
    } else {
        None
    };
    let shared_str = shared.to_str().context("shared-dir non UTF-8")?;
    let mut cmd = match &backend {
        Some(ReadOnlyBackend::Bwrap(bwrap)) => {
            let mut c = Command::new(bwrap);
            c.args(["--dev-bind", "/", "/", "--ro-bind", shared_str, shared_str])
                .arg("--")
                .arg(&virtiofsd);
            c
        }
        _ => Command::new(&virtiofsd),
    };
    if backend == Some(ReadOnlyBackend::Native) {
        cmd.arg("--readonly");
    }
    cmd.args([
        "--sandbox=none",
        "--socket-path",
        socket.to_str().context("socket path non UTF-8")?,
        "--shared-dir",
        shared_str,
        "--killpriv-v2",
        "--cache=auto",
    ]);
    if let Some(group) = libvirt_access::virtiofs_socket_group() {
        cmd.args(["--socket-group", group]);
    }
    cmd.stdin(Stdio::null())
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
            // Sous bubblewrap, on suit le virtiofsd lui-même (son enfant) : c'est lui que
            // l'état reconnaît et arrête ; bwrap se termine avec lui.
            if matches!(backend, Some(ReadOnlyBackend::Bwrap(_))) {
                let needle = socket.to_string_lossy();
                if let Some(pid) = crate::procs::find_matching("virtiofsd", &needle)
                    .into_iter()
                    .find(|p| *p != daemon_pid)
                {
                    return Ok(pid);
                }
                bail!(
                    "virtiofsd introuvable sous bubblewrap pour le volume '{mount_name}' ({}).",
                    socket.display()
                );
            }
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
    crate::procs::alive(pid)
}

/// Lit `mounts-state.json` (fichier de l'utilisateur, pas un lien) et revalide les noms
/// de volume : ils sont réinjectés dans le XML du domaine et les commandes de montage.
pub fn load_mounts_state(instance_dir: &Path) -> Result<Vec<ResolvedMount>> {
    let path = instance_dir.join(MOUNTS_STATE_FILE);
    let Some(content) = crate::fsutil::read_owned(&path)? else {
        return Ok(Vec::new());
    };
    let mounts: Vec<ResolvedMount> =
        serde_json::from_str(&content).with_context(|| format!("{} invalide", path.display()))?;
    for m in &mounts {
        if validate_volume_name(&m.name)? != m.name {
            bail!("{} : nom de volume '{}' invalide.", path.display(), m.name);
        }
        resolve_guest_path(&m.guest_path)
            .with_context(|| format!("{} : chemin invité invalide", path.display()))?;
        if m.socket_path != PathBuf::new()
            && m.socket_path != socket_path_for(instance_dir, &m.name)
        {
            bail!(
                "{} : socket inattendu pour '{}' ({}).",
                path.display(),
                m.name,
                m.socket_path.display()
            );
        }
    }
    Ok(mounts)
}

fn save_mounts_state(instance_dir: &Path, mounts: &[ResolvedMount]) -> Result<()> {
    let path = instance_dir.join(MOUNTS_STATE_FILE);
    let content = serde_json::to_string_pretty(mounts).context("sérialisation mounts-state")?;
    crate::fsutil::write_atomic(&path, format!("{content}\n").as_bytes(), 0o644)
}

/// Arrête un virtiofsd seulement s'il s'agit bien du nôtre (même utilisateur, même socket).
fn stop_virtiofsd_pid(pid: u32, socket_path: &Path) -> bool {
    crate::procs::signal_if_matches(
        pid,
        libc::SIGTERM,
        "virtiofsd",
        &socket_path.to_string_lossy(),
    )
}

/// Répertoires hôte des mounts d'une instance et racine autorisée pour `--purge`
/// (répertoire du Virtumfile enregistré à la création).
pub fn purge_targets_from_instance(instance_dir: &Path) -> Result<(Vec<PathBuf>, PathBuf)> {
    let mounts = load_mounts_state(instance_dir)?;
    let dirs: Vec<PathBuf> = mounts.iter().map(|m| m.host_abs.clone()).collect();
    let mut roots = mounts.iter().map(|m| m.virtumfile_dir.clone());
    let root = match roots.next() {
        None => bail!("--purge : aucun dossier mount enregistré pour cette VM"),
        Some(first) => first,
    };
    let root = match root {
        Some(r) if mounts.iter().all(|m| m.virtumfile_dir.as_ref() == Some(&r)) => r,
        _ => bail!(
            "--purge : répertoire du Virtumfile non enregistré pour cette VM (état antérieur). \
             Utilisez `virtum down --destroy --purge -f <Virtumfile>`, ou supprimez les dossiers \
             à la main."
        ),
    };
    Ok((dirs, root))
}

/// Refuse qu'un partage déclaré (`new`) soit imbriqué avec un partage servi par une autre
/// instance de cet état, ou par cette instance (partage retiré du Virtumfile mais actif),
/// et qu'un partage inscriptible contienne le Virtumfile d'une autre instance (ou
/// l'inverse). `ensure_mounts_not_nested` ne voit qu'un Virtumfile à la fois ; un invité
/// qui écrit le partage englobant pourrait remplacer le dossier inclus par un lien.
pub async fn ensure_not_nested_across_instances(
    paths: &crate::config::VirtumPaths,
    new: &[(String, PathBuf, bool)],
    virtumfile_dir: &Path,
) -> Result<()> {
    let metadata = crate::metadata::load_metadata(&paths.metadata).await?;
    let vf = virtumfile_dir
        .canonicalize()
        .unwrap_or_else(|_| virtumfile_dir.to_path_buf());
    for inst in &metadata.instances {
        let dir = crate::vm::instance_dir_for(paths, &inst.id)?;
        if !dir.is_dir() {
            continue;
        }
        for m in load_mounts_state(&dir)? {
            let label = format!(
                "{}/{}",
                inst.machine_name.as_deref().unwrap_or(&inst.id),
                m.name
            );
            for (new_label, path, rw) in new {
                if *path != m.host_abs
                    && (path.starts_with(&m.host_abs) || m.host_abs.starts_with(path))
                {
                    bail!(
                        "Mounts imbriqués interdits : '{new_label}' ({}) et le partage actif \
                         '{label}' ({}) se recouvrent. Un invité pourrait remplacer le dossier \
                         inclus par un lien symbolique ; utilisez des dossiers disjoints (ou \
                         détruisez l'instance qui sert l'autre partage).",
                        path.display(),
                        m.host_abs.display()
                    );
                }
                if *rw {
                    if let Some(other_vf) = &m.virtumfile_dir {
                        if other_vf.starts_with(path) {
                            bail!(
                                "Mount '{new_label}' ({}) est inscriptible et contient le projet \
                                 d'une autre instance ({}) : refus.",
                                path.display(),
                                other_vf.display()
                            );
                        }
                    }
                }
            }
            if !m.read_only && vf.starts_with(&m.host_abs) {
                bail!(
                    "Le Virtumfile ({}) est dans le partage inscriptible actif '{label}' ({}) : \
                     un invité pourrait le modifier. Déplacez le projet hors de ce partage.",
                    vf.display(),
                    m.host_abs.display()
                );
            }
        }
    }
    Ok(())
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

/// Supprime les répertoires hôte déclarés dans `mounts:` (option `--purge` au destroy).
/// Ne supprime que des sous-répertoires stricts de `allowed_root` (répertoire du Virtumfile).
pub fn purge_host_mount_dirs(dirs: &[PathBuf], allowed_root: &Path) -> Result<usize> {
    let base = allowed_root
        .canonicalize()
        .with_context(|| format!("Répertoire Virtumfile invalide: {}", allowed_root.display()))?;
    // La racine et les dossiers enregistrés doivent encore se résoudre vers eux-mêmes :
    // un lien posé par un invité (via un partage englobant) déplacerait sinon la racine
    // et les cibles ensemble, et la suppression sortirait de tout partage.
    if base != allowed_root {
        bail!(
            "--purge : le répertoire du Virtumfile {} résout maintenant vers {} (lien \
             symbolique ?) : suppression refusée.",
            allowed_root.display(),
            base.display()
        );
    }
    let mut purged = 0usize;
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let canonical = dir
            .canonicalize()
            .with_context(|| format!("Impossible de résoudre {}", dir.display()))?;
        if canonical != *dir {
            bail!(
                "--purge : {} résout maintenant vers {} (lien symbolique ?) : suppression refusée.",
                dir.display(),
                canonical.display()
            );
        }
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
        crate::verr!("Dossier mount supprimé : {}", canonical.display());
        purged += 1;
    }
    Ok(purged)
}

/// Arrête les virtiofsd et supprime sockets / état (destroy / cleanup).
pub fn teardown_virtiofs(instance_dir: &Path) {
    let mounts = load_mounts_state(instance_dir).unwrap_or_default();
    for mount in &mounts {
        if let Some(pid) = mount.virtiofsd_pid {
            stop_virtiofsd_pid(pid, &mount.socket_path);
        }
        let _ = std::fs::remove_file(socket_path_for(instance_dir, &mount.name));
    }
    let state = instance_dir.join(MOUNTS_STATE_FILE);
    let _ = std::fs::remove_file(state);
    // virtiofsd restants de cette instance (état perdu) : uniquement les nôtres, servant
    // un socket de ce répertoire (pas de regex `pkill -f` construite depuis un chemin).
    let needle = format!("{}/virtiofs-", instance_dir.display());
    for pid in crate::procs::find_matching("virtiofsd", &needle) {
        crate::procs::signal_if_matches(pid, libc::SIGTERM, "virtiofsd", &needle);
    }
}

pub fn mounts_to_instance_specs(mounts: &[ResolvedMount]) -> Vec<MountSpec> {
    mounts
        .iter()
        .map(|m| MountSpec {
            name: m.name.clone(),
            host: m.host.clone(),
            guest: m.guest.clone(),
            rw: !m.read_only,
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
            } else if s.rw != d.rw {
                errors.push(format!(
                    "volume '{}' : accès modifié ({} → {}). \
                     Utilisez virtum destroy puis virtum up.",
                    d.name,
                    access_label(s.rw),
                    access_label(d.rw)
                ));
            }
        }
    }
    errors
}

fn access_label(rw: bool) -> &'static str {
    if rw {
        "lecture-écriture"
    } else {
        "lecture seule"
    }
}

/// Étapes de montage virtiofs exécutées via SSH (chemins déjà validés).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSshStep {
    Mkdir {
        path: String,
    },
    Mount {
        volume: String,
        path: String,
        read_only: bool,
    },
    Chown {
        path: String,
    },
}

/// Prépare les étapes SSH pour monter des volumes (sans concaténation shell).
pub fn ssh_mount_steps(mounts: &[ResolvedMount]) -> Vec<MountSshStep> {
    let mut steps = Vec::new();
    for m in mounts {
        steps.push(MountSshStep::Mkdir {
            path: m.guest_path.clone(),
        });
        steps.push(MountSshStep::Mount {
            volume: m.name.clone(),
            path: m.guest_path.clone(),
            read_only: m.read_only,
        });
        // Lecture seule : l'hôte refuse toute écriture, chown compris.
        if !m.read_only {
            steps.push(MountSshStep::Chown {
                path: m.guest_path.clone(),
            });
        }
    }
    steps
}

/// Commandes shell legacy — préférez `ssh_mount_steps` + `ssh::run_ssh_mount_steps`.
#[deprecated(note = "use ssh_mount_steps with ssh::run_ssh_mount_steps")]
pub fn ssh_mount_commands(mounts: &[ResolvedMount]) -> Vec<String> {
    ssh_mount_steps(mounts)
        .into_iter()
        .map(|step| match step {
            MountSshStep::Mkdir { path } => format!("mkdir -p -- {path}"),
            MountSshStep::Mount {
                volume,
                path,
                read_only,
            } => {
                let ro = if read_only { "-o ro " } else { "" };
                format!("mount -t virtiofs {ro}{volume} {path} || true")
            }
            MountSshStep::Chown { path } => format!("chown -R virtum:virtum {path}"),
        })
        .collect()
}

pub fn format_mount_list(mounts: &[MountSpec]) -> String {
    if mounts.is_empty() {
        return "-".to_string();
    }
    mounts
        .iter()
        .map(|m| {
            let rw = if m.rw { " (rw)" } else { "" };
            format!("{}:{}→{}{rw}", m.name, m.host, m.guest)
        })
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
    fn resolve_guest_path_rejects_contract_cases() {
        let cases = [
            ("/home/virtum-evil", "prefix bypass"),
            ("/home/virtum/app;id", "semicolon"),
            ("/home/virtum/a b", "space"),
            ("~/app\n-evil", "newline"),
            ("/etc/passwd", "outside home"),
            ("", "empty"),
        ];
        for (input, _label) in cases {
            let err = resolve_guest_path(input).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("Chemin invité") && msg.contains("invalide"),
                "input={input:?} msg={msg}"
            );
        }
    }

    /// Lecture seule par défaut ; `rw` explicite (forme courte ou détaillée).
    #[test]
    fn mounts_are_read_only_unless_rw() {
        let parse = |yaml: &str| {
            let raw: Vec<serde_yaml::Value> = serde_yaml::from_str(yaml).unwrap();
            parse_mount_entries(&raw).unwrap()
        };
        let m = parse(
            "- ./src:~/src\n- ./out:~/out:rw\n- ./doc:~/doc:ro\n\
             - {name: data, host: ./data, guest: ~/data}\n\
             - {name: db, host: ./db, guest: ~/db, rw: true}\n",
        );
        let got: Vec<_> = m.iter().map(|m| (m.guest.as_str(), m.rw)).collect();
        assert_eq!(
            got,
            [
                ("~/src", false),
                ("~/out", true),
                ("~/doc", false),
                ("~/data", false),
                ("~/db", true)
            ]
        );
        // Un changement d'accès demande de recréer la VM.
        let mut rw = m[0].clone();
        rw.rw = true;
        let msg = mounts_changed_blocking(&m[..1], &[rw]);
        assert!(
            msg[0].contains("lecture seule → lecture-écriture"),
            "{msg:?}"
        );
        // État antérieur sans le champ : le démon qui tourne a été lancé en écriture.
        let legacy: ResolvedMount = serde_json::from_str(
            r#"{"name":"a","host":"./a","guest":"~/a","host_abs":"/p/a",
                "guest_path":"/home/virtum/a","socket_path":"/s"}"#,
        )
        .unwrap();
        assert!(!legacy.read_only);
    }

    /// Le partage en lecture seule est imposé côté hôte : l'écriture échoue sous bubblewrap.
    #[test]
    fn bwrap_read_only_bind_blocks_writes() {
        let Some(bwrap) = resolve_bwrap_path() else {
            return;
        };
        let d = scratch("ro-bind");
        let share = d.join("share");
        fs::create_dir_all(&share).unwrap();
        let s = share.to_str().unwrap();
        if bwrap_unusable(&bwrap) {
            let _ = fs::remove_dir_all(d);
            return; // espaces de noms refusés sur cette machine
        }
        let wrote = Command::new(&bwrap)
            .args(["--dev-bind", "/", "/", "--ro-bind", s, s, "--", "sh", "-c"])
            .arg(format!("echo x > {s}/f"))
            .status()
            .unwrap();
        assert!(!wrote.success());
        assert!(!share.join("f").exists());
        let _ = fs::remove_dir_all(d);
    }

    fn bwrap_unusable(bwrap: &Path) -> bool {
        !Command::new(bwrap)
            .args(["--dev-bind", "/", "/", "true"])
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn ssh_mount_steps_use_validated_paths() {
        let mounts = vec![ResolvedMount {
            name: "app".into(),
            host: "./app".into(),
            guest: "~/app".into(),
            host_abs: PathBuf::from("/tmp/app"),
            guest_path: "/home/virtum/app".into(),
            socket_path: PathBuf::new(),
            virtiofsd_pid: None,
            virtumfile_dir: None,
            read_only: false,
        }];
        let steps = ssh_mount_steps(&mounts);
        assert_eq!(steps.len(), 3);
        assert!(matches!(
            &steps[0],
            MountSshStep::Mkdir { path } if path == "/home/virtum/app"
        ));
        // Lecture seule : monté `-o ro`, sans chown (l'hôte refuserait l'écriture).
        let ro = vec![ResolvedMount {
            read_only: true,
            ..mounts[0].clone()
        }];
        let steps = ssh_mount_steps(&ro);
        assert_eq!(steps.len(), 2);
        assert!(matches!(
            &steps[1],
            MountSshStep::Mount {
                read_only: true,
                ..
            }
        ));
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

    fn scratch(tag: &str) -> PathBuf {
        crate::test_env::private_temp_dir(&format!("mount-{tag}"))
            .canonicalize()
            .unwrap()
    }

    /// Audit run-4 : une racine de purge remplacée par un lien (invité d'un lab englobant)
    /// ne doit pas déplacer la suppression hors des partages.
    #[test]
    fn purge_refuses_redirected_root() {
        let d = scratch("purge-redirect");
        let victim = d.join("data");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep.txt"), "x").unwrap();
        let ws = d.join("ws");
        fs::create_dir_all(ws.join("projb.orig/data")).unwrap();
        // L'invité a remplacé ws/projb par un lien vers l'ancêtre `d`.
        std::os::unix::fs::symlink("..", ws.join("projb")).unwrap();
        let root = ws.join("projb");
        let err = purge_host_mount_dirs(&[root.join("data")], &root).unwrap_err();
        assert!(format!("{err:#}").contains("lien symbolique"), "{err:#}");
        assert!(victim.join("keep.txt").exists());
        // Cas normal : racine et cible canoniques, suppression faite.
        let real = ws.join("projb.orig");
        assert_eq!(
            purge_host_mount_dirs(&[real.join("data")], &real).unwrap(),
            1
        );
        let _ = fs::remove_dir_all(d);
    }

    /// Audit : un lien symbolique dans le chemin ne doit rien créer hors du Virtumfile.
    #[test]
    fn resolve_host_symlink_creates_nothing_outside() {
        let d = scratch("symlink");
        let proj = d.join("proj");
        let outside = d.join("outside");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, proj.join("link")).unwrap();

        let err = resolve_host_path(&proj, "./link/created-by-virtum").unwrap_err();
        assert!(format!("{err:#}").contains("lien symbolique"), "{err:#}");
        assert!(!outside.join("created-by-virtum").exists());

        // Lien vers l'intérieur du projet (ex. data/cache -> ..) : refusé aussi.
        fs::create_dir_all(proj.join("data")).unwrap();
        std::os::unix::fs::symlink("..", proj.join("data/cache")).unwrap();
        assert!(resolve_host_path(&proj, "./data/cache").is_err());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn nested_mounts_are_rejected_but_identical_paths_allowed() {
        let a = PathBuf::from("/p/data");
        let b = PathBuf::from("/p/data/cache");
        assert!(
            ensure_mounts_not_nested(&[("w/data".into(), a.clone()), ("w/cache".into(), b)])
                .is_err()
        );
        assert!(
            ensure_mounts_not_nested(&[("w/data".into(), a.clone()), ("db/data".into(), a)])
                .is_ok()
        );
        assert!(ensure_mounts_not_nested(&[
            ("w/a".into(), PathBuf::from("/p/data")),
            ("w/b".into(), PathBuf::from("/p/data2")),
        ])
        .is_ok());
    }

    /// Audit : un partage remplacé par un lien symbolique après la création est refusé
    /// avant de relancer virtiofsd.
    #[test]
    fn stored_share_replaced_by_symlink_is_refused() {
        let d = scratch("swap");
        let proj = d.join("proj");
        let outside = d.join("outside_home");
        fs::create_dir_all(proj.join("data")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let mount = ResolvedMount {
            name: "data".into(),
            host: "./data".into(),
            guest: "~/data".into(),
            host_abs: proj.join("data"),
            guest_path: "/home/virtum/data".into(),
            socket_path: PathBuf::new(),
            virtiofsd_pid: None,
            virtumfile_dir: Some(proj.clone()),
            read_only: true,
        };
        validate_stored_share(&mount).unwrap();
        fs::remove_dir(proj.join("data")).unwrap();
        std::os::unix::fs::symlink(&outside, proj.join("data")).unwrap();
        assert!(validate_stored_share(&mount).is_err());

        // Chemin réel mais hors de la racine enregistrée : refusé.
        let escaped = ResolvedMount {
            host_abs: outside.clone(),
            ..mount
        };
        assert!(validate_stored_share(&escaped).is_err());
        let _ = fs::remove_dir_all(d);
    }

    /// Audit : la racine d'un partage n'est plus accessible aux autres comptes locaux.
    #[test]
    fn share_root_loses_other_access() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("share-mode");
        let share = d.join("share");
        fs::create_dir_all(&share).unwrap();
        fs::set_permissions(&share, fs::Permissions::from_mode(0o2775)).unwrap();
        restrict_share_root("share", &share).unwrap();
        let mode = fs::metadata(&share).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o2770);
        // Idempotent.
        restrict_share_root("share", &share).unwrap();
        let _ = fs::remove_dir_all(d);
    }

    /// Audit : une ACL nommée qui ouvre la traversée du partage fait refuser le partage.
    #[test]
    fn share_root_with_named_acl_is_refused() {
        let d = scratch("share-acl");
        let share = d.join("share");
        fs::create_dir_all(&share).unwrap();
        let ok = std::process::Command::new("setfacl")
            .args(["-m", "u:65534:x"])
            .arg(&share)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let err = restrict_share_root("share", &share).unwrap_err();
            assert!(format!("{err:#}").contains("ACL"), "{err:#}");
            // ACL retirée : le partage est accepté.
            std::process::Command::new("setfacl")
                .args(["-b"])
                .arg(&share)
                .status()
                .unwrap();
            restrict_share_root("share", &share).unwrap();
        }
        let _ = fs::remove_dir_all(d);
    }

    /// Audit : l'accès groupe est retiré quand le groupe du dossier contient d'autres comptes.
    #[test]
    fn share_root_loses_shared_group_access() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let d = scratch("share-group");
        let share = d.join("share");
        fs::create_dir_all(&share).unwrap();
        // kvm contient souvent libvirt-qemu ; ignoré si on n'en est pas membre.
        let chgrp_ok = std::process::Command::new("chgrp")
            .arg("kvm")
            .arg(&share)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if chgrp_ok {
            fs::set_permissions(&share, fs::Permissions::from_mode(0o775)).unwrap();
            let gid = fs::metadata(&share).unwrap().gid();
            let shared =
                crate::share_access::group_has_other_members(gid, crate::fsutil::current_uid());
            restrict_share_root("share", &share).unwrap();
            let mode = fs::metadata(&share).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, if shared { 0o700 } else { 0o770 });
        }
        let _ = fs::remove_dir_all(d);
    }

    /// Audit : la racine de `--purge` vient de l'état enregistré, pas des chemins eux-mêmes ;
    /// un état sans racine (ancien) refuse la purge.
    #[test]
    fn purge_root_comes_from_recorded_virtumfile_dir() {
        let d = scratch("purge");
        let inst = d.join("inst");
        fs::create_dir_all(&inst).unwrap();
        let m = |name: &str, root: Option<PathBuf>| ResolvedMount {
            name: name.into(),
            host: format!("./{name}"),
            guest: format!("~/{name}"),
            host_abs: d.join("proj").join(name),
            guest_path: format!("/home/virtum/{name}"),
            socket_path: PathBuf::new(),
            virtiofsd_pid: None,
            virtumfile_dir: root,
            read_only: true,
        };
        save_mounts_state(
            &inst,
            &[m("a", Some(d.join("proj"))), m("b", Some(d.join("proj")))],
        )
        .unwrap();
        let (dirs, root) = purge_targets_from_instance(&inst).unwrap();
        assert_eq!(root, d.join("proj"));
        assert_eq!(dirs.len(), 2);
        save_mounts_state(&inst, &[m("a", None)]).unwrap();
        assert!(purge_targets_from_instance(&inst).is_err());
        let _ = fs::remove_dir_all(d);
    }

    /// Audit : un nom de volume altéré (saut de ligne) dans mounts-state.json est refusé.
    #[test]
    fn tampered_mount_name_in_state_is_rejected() {
        let d = scratch("names");
        let state = d.join(MOUNTS_STATE_FILE);
        fs::write(
            &state,
            r#"[{"name":"z\nssh-ed25519 AAAA x","host":"./z","guest":"~/z","host_abs":"/tmp/z","guest_path":"/home/virtum/z","socket_path":""}]"#,
        )
        .unwrap();
        assert!(load_mounts_state(&d).is_err());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn parse_mount_shortcut_entry() {
        let spec = mount_entry_to_spec(MountYamlEntry::Short("./backend:~/app".into()), 0).unwrap();
        assert_eq!(spec.host, "./backend");
        assert_eq!(spec.guest, "~/app");
        assert_eq!(spec.name, "backend");
    }
}
