//! Provisionnement Ansible d'un lab depuis l'hôte (`ansible:`, `virtum provision`, v0.8).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{self, VirtumPaths};
use crate::inventory;
use crate::ssh::{self, SshSessionOpts};
use crate::virtumfile::{self, Virtumfile};

/// État du provisionnement d'un lab (`ansible-<lab>.json`, à côté de `firewall-<lab>.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsibleState {
    /// Date du dernier provisionnement réussi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioned_at: Option<String>,
    /// `success` ou `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<String>,
    #[serde(default)]
    pub playbook: String,
    /// Empreinte du fichier requirements lors de la dernière installation réussie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements_sha256: Option<String>,
    /// Instantané pris après provisionnement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

impl AnsibleState {
    pub fn succeeded(&self) -> bool {
        self.last_run.as_deref() == Some("success")
    }
}

pub fn state_path(paths: &VirtumPaths, lab_name: &str) -> PathBuf {
    paths.root.join(format!("ansible-{lab_name}.json"))
}

/// Inventaire utilisé par le dernier provisionnement (réécrit à chaque exécution,
/// réutilisable à la main : `ansible -i <ce fichier> …`).
pub fn inventory_path(paths: &VirtumPaths, lab_name: &str) -> PathBuf {
    paths.root.join(format!("ansible-{lab_name}.inventory.ini"))
}

pub fn load_state(paths: &VirtumPaths, lab_name: &str) -> Result<Option<AnsibleState>> {
    let path = state_path(paths, lab_name);
    let Some(text) = crate::fsutil::read_owned(&path)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&text).with_context(|| {
        format!("État Ansible illisible : {}", path.display())
    })?))
}

pub fn save_state(paths: &VirtumPaths, lab_name: &str, state: &AnsibleState) -> Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    inventory::write_atomic(&state_path(paths, lab_name), &format!("{text}\n"))
}

/// Supprime l'état et l'inventaire du lab (`virtum down --destroy`). Idempotent.
pub fn remove_state(paths: &VirtumPaths, lab_name: &str) {
    let _ = std::fs::remove_file(state_path(paths, lab_name));
    let _ = std::fs::remove_file(inventory_path(paths, lab_name));
    remove_workspace(paths, lab_name);
}

/// Raison de jouer le playbook à la fin de `up`, `None` s'il ne faut pas le rejouer.
pub fn provision_reason(
    previous: Option<&AnsibleState>,
    created_machines: bool,
) -> Option<&'static str> {
    match previous {
        None => return Some("lab non provisionné"),
        Some(state) if !state.succeeded() => return Some("dernier provisionnement en échec"),
        Some(_) => {}
    }
    if created_machines {
        Some("machines ajoutées")
    } else {
        None
    }
}

/// Programme présent dans le PATH.
pub fn command_in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
                && std::fs::metadata(&candidate)
                    .map(|m| {
                        use std::os::unix::fs::PermissionsExt;
                        m.permissions().mode() & 0o111 != 0
                    })
                    .unwrap_or(false)
        })
    })
}

/// Outils Ansible requis par la section `ansible:` (erreur claire sinon).
pub fn require_tools(spec: &virtumfile::AnsibleSpec) -> Result<()> {
    if !command_in_path("ansible-playbook") {
        bail!(
            "`ansible-playbook` est introuvable : installez Ansible (ex. `sudo apt install ansible-core`) \
             ou retirez la section `ansible:` du Virtumfile."
        );
    }
    if spec.requirements.is_some() && !command_in_path("ansible-galaxy") {
        bail!("`ansible-galaxy` est introuvable alors que `ansible.requirements` est déclaré.");
    }
    Ok(())
}

/// Ce que contient un fichier requirements : `(collections, rôles)`.
pub fn requirements_kinds(content: &str) -> (bool, bool) {
    match serde_yaml::from_str::<serde_yaml::Value>(content) {
        Ok(serde_yaml::Value::Mapping(map)) => {
            let has = |key: &str| {
                map.get(key)
                    .and_then(|v| v.as_sequence())
                    .is_some_and(|s| !s.is_empty())
            };
            (has("collections"), has("roles"))
        }
        // Ancien format : liste de rôles à la racine.
        Ok(serde_yaml::Value::Sequence(seq)) => (false, !seq.is_empty()),
        _ => (false, false),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Résultat d'une exécution du playbook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Success,
    /// Code de sortie d'`ansible-playbook` (130 après Ctrl-C).
    Failed(i32),
}

/// Toutes les machines du Virtumfile tournent et répondent en SSH ; écrit l'inventaire.
async fn prepare_inventory(
    lab: &Virtumfile,
    paths: &VirtumPaths,
    insecure_ssh: bool,
) -> Result<PathBuf> {
    let (inv, warnings) = inventory::lab_inventory(lab, paths, insecure_ssh).await?;
    if !warnings.is_empty() {
        for w in &warnings {
            eprintln!("  ✗ {w}");
        }
        bail!("Toutes les machines du lab doivent tourner avant le provisionnement Ansible.");
    }
    let private_key = ssh::private_key_path(&paths.keys);
    let mut unreachable = Vec::new();
    for host in &inv.hosts {
        let var = |k: &str| {
            host.vars
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let opts = SshSessionOpts::new(&paths.instances.join(var("virtum_vm_id")), insecure_ssh);
        let port = var("ansible_port")
            .parse()
            .unwrap_or(virtumfile::DEFAULT_SSH_PORT);
        if !ssh::ssh_auth_probe(
            &private_key,
            &var("ansible_user"),
            &var("ansible_host"),
            port,
            &opts,
        ) {
            unreachable.push(format!("{} ({}:{port})", host.name, var("ansible_host")));
        }
    }
    if !unreachable.is_empty() {
        bail!(
            "Machine(s) injoignable(s) en SSH : {}. Vérifiez avec `virtum shell`.",
            unreachable.join(", ")
        );
    }
    let path = inventory_path(paths, &lab.name);
    inventory::write_atomic(&path, &inventory::render_ini(&inv))?;
    Ok(path)
}

/// Lance un programme en héritant stdin/stdout/stderr. Ctrl-C : le signal atteint aussi
/// l'enfant (même groupe de processus) ; on attend sa fin pour enregistrer l'échec.
async fn run_inherited(mut cmd: tokio::process::Command) -> Result<i32> {
    let mut child = cmd.spawn().context("Impossible de lancer la commande")?;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nInterruption : attente de la fin d'Ansible…");
            }
        }
    };
    Ok(status.code().unwrap_or(130))
}

/// Chemins locaux candidats cités par un fichier requirements (sources `type: dir|file`,
/// rôles `src: ./…`, `file://…`) : toute chaîne qui ressemble à un chemin.
fn requirements_local_paths(content: &str) -> Vec<String> {
    fn walk(v: &serde_yaml::Value, out: &mut Vec<String>) {
        match v {
            serde_yaml::Value::String(s) => {
                let s = s.trim();
                let s = s.strip_prefix("file://").unwrap_or(s);
                if s.starts_with('/') || s.starts_with('.') || s.starts_with('~') || s.contains('/')
                {
                    out.push(s.to_string());
                }
            }
            serde_yaml::Value::Sequence(seq) => seq.iter().for_each(|x| walk(x, out)),
            serde_yaml::Value::Mapping(map) => map.values().for_each(|x| walk(x, out)),
            serde_yaml::Value::Tagged(t) => walk(&t.value, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(content) {
        walk(&v, &mut out);
    }
    out
}

/// Refuse une source locale de `requirements` située dans un partage inscriptible :
/// ansible-galaxy l'installerait dans `~/.ansible`, d'où ansible-playbook la chargerait.
/// Les chemins relatifs sont résolus depuis le dossier courant, celui du Virtumfile et
/// celui du fichier requirements (les trois bases possibles pour ansible-galaxy).
fn ensure_requirements_sources_outside_shares(
    content: &str,
    bases: &[PathBuf],
    shares: &[(String, PathBuf)],
) -> Result<()> {
    for raw in requirements_local_paths(content) {
        let candidates: Vec<PathBuf> = if Path::new(&raw).is_absolute() {
            vec![PathBuf::from(&raw)]
        } else {
            bases.iter().map(|b| b.join(&raw)).collect()
        };
        for c in candidates {
            let resolved = c.canonicalize().unwrap_or(c);
            if let Some((label, share)) = shares.iter().find(|(_, s)| resolved.starts_with(s)) {
                bail!(
                    "ansible.requirements : la source locale '{raw}' ({}) est dans le partage \
                     inscriptible '{label}' ({}) : ansible-galaxy installerait du code écrit \
                     par l'invité. Retirez `rw` de ce partage ou déplacez la source.",
                    resolved.display(),
                    share.display()
                );
            }
        }
    }
    Ok(())
}

/// Installe les requirements si le fichier a changé depuis la dernière installation réussie.
async fn install_requirements(
    requirements: &Path,
    vf_dir: &Path,
    shares: &[(String, PathBuf)],
    state: &mut AnsibleState,
) -> Result<()> {
    let content = std::fs::read(requirements)
        .with_context(|| format!("Impossible de lire {}", requirements.display()))?;
    let mut bases = vec![vf_dir.to_path_buf()];
    if let Some(dir) = requirements.parent() {
        bases.push(dir.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    ensure_requirements_sources_outside_shares(&String::from_utf8_lossy(&content), &bases, shares)?;
    let sha = sha256_hex(&content);
    if state.requirements_sha256.as_deref() == Some(sha.as_str()) {
        return Ok(());
    }
    let (collections, roles) = requirements_kinds(&String::from_utf8_lossy(&content));
    for (kind, wanted) in [("collection", collections), ("role", roles)] {
        if !wanted {
            continue;
        }
        eprintln!(
            "  ansible-galaxy {kind} install -r {}",
            requirements.display()
        );
        let mut cmd = tokio::process::Command::new("ansible-galaxy");
        cmd.arg(kind).arg("install").arg("-r").arg(requirements);
        let code = run_inherited(cmd).await?;
        if code != 0 {
            bail!("ansible-galaxy {kind} install a échoué (code {code}).");
        }
    }
    state.requirements_sha256 = Some(sha);
    Ok(())
}

/// Partages inscriptibles par un invité : ceux déclarés `rw` dans le Virtumfile et tous
/// ceux que servent les instances Virtum de cet état (tout lab confondu, y compris ceux
/// retirés du Virtumfile mais encore actifs). Chemins canoniques.
async fn writable_shares(
    lab: &Virtumfile,
    vf_dir: &Path,
    paths: &VirtumPaths,
) -> Result<Vec<(String, PathBuf)>> {
    let mut shares = Vec::new();
    for machine in lab.machines.keys() {
        for m in crate::mounts::resolve_machine_mounts(lab, machine, vf_dir)? {
            if !m.read_only {
                shares.push((format!("{machine}/{}", m.name), m.host_abs));
            }
        }
    }
    let metadata = crate::metadata::load_metadata(&paths.metadata).await?;
    for inst in &metadata.instances {
        let dir = crate::vm::instance_dir_for(paths, &inst.id)?;
        if !dir.is_dir() {
            continue;
        }
        for m in crate::mounts::load_mounts_state(&dir)? {
            if !m.read_only {
                let machine = inst.machine_name.as_deref().unwrap_or(&inst.id);
                shares.push((format!("{machine}/{}", m.name), m.host_abs));
            }
        }
    }
    for (_, path) in shares.iter_mut() {
        if let Ok(canonical) = path.canonicalize() {
            *path = canonical;
        }
    }
    Ok(shares)
}

/// Taille maximale de la copie privée du dossier du playbook.
const SNAPSHOT_MAX_BYTES: u64 = 1 << 30;

/// Dossier de travail Ansible du lab : copie privée du dossier du playbook, recréée à
/// chaque exécution.
pub fn workspace_path(paths: &VirtumPaths, lab_name: &str) -> PathBuf {
    paths.root.join("ansible-work").join(lab_name)
}

/// Résultat de [`snapshot_project`].
#[derive(Debug, Default)]
pub struct SnapshotReport {
    /// Chemins (relatifs au dossier copié) non copiés car dans un partage inscriptible.
    pub skipped: Vec<PathBuf>,
    pub bytes: u64,
}

/// Copie le dossier `root` (canonique) dans `dest` (à créer) pour qu'`ansible-playbook`
/// n'exécute rien qu'un invité puisse modifier :
/// - tout ce qui se trouve dans un partage inscriptible (`excluded`, canoniques) est omis,
///   y compris à travers un lien symbolique ;
/// - un lien qui pointe dans `root` est suivi (son contenu est copié) ; un lien qui en
///   sort devient un lien absolu vers sa cible réelle (hors de tout partage) ;
/// - `.git`, les sockets, FIFO et périphériques sont ignorés.
///
/// La copie est figée : l'invité ne peut plus rien y changer pendant l'exécution.
pub fn snapshot_project(root: &Path, dest: &Path, excluded: &[PathBuf]) -> Result<SnapshotReport> {
    if excluded.iter().any(|e| root.starts_with(e)) {
        bail!(
            "Le répertoire du Virtumfile ({}) est dans un partage inscriptible.",
            root.display()
        );
    }
    create_private_dir(dest)?;
    let mut report = SnapshotReport::default();
    let mut ancestors = vec![root.to_path_buf()];
    copy_tree(
        root,
        dest,
        Path::new(""),
        root,
        excluded,
        &mut ancestors,
        &mut report,
    )?;
    Ok(report)
}

fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("Impossible de créer {}", path.display()))
}

fn copy_tree(
    src: &Path,
    dest: &Path,
    rel: &Path,
    root: &Path,
    excluded: &[PathBuf],
    ancestors: &mut Vec<PathBuf>,
    report: &mut SnapshotReport,
) -> Result<()> {
    let entries =
        std::fs::read_dir(src).with_context(|| format!("Impossible de lire {}", src.display()))?;
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = src.join(&name);
        let rel_entry = rel.join(&name);
        let target = dest.join(&name);
        // Cible réelle : un lien ou un dossier qui mène dans un partage est écarté.
        let Ok(canonical) = path.canonicalize() else {
            continue; // lien cassé
        };
        if excluded.iter().any(|e| canonical.starts_with(e)) {
            report.skipped.push(rel_entry);
            continue;
        }
        let link = std::fs::symlink_metadata(&path)?.file_type().is_symlink();
        if link && !canonical.starts_with(root) {
            // Le lien resterait actif dans la copie : sa cible ne doit contenir aucun
            // partage inscriptible, sinon ce que l'invité y écrit serait chargé.
            if let Some(share) = excluded.iter().find(|e| e.starts_with(&canonical)) {
                bail!(
                    "{} pointe vers {}, qui contient le partage inscriptible {} : copie Ansible \
                     refusée. Retirez `rw` de ce partage ou remplacez le lien par un dossier.",
                    rel_entry.display(),
                    canonical.display(),
                    share.display()
                );
            }
            std::os::unix::fs::symlink(&canonical, &target)
                .with_context(|| format!("Impossible de créer {}", target.display()))?;
            continue;
        }
        let meta = std::fs::metadata(&canonical)?;
        if meta.is_dir() {
            if ancestors.contains(&canonical) {
                continue; // boucle de liens
            }
            create_private_dir(&target)?;
            ancestors.push(canonical.clone());
            let result = copy_tree(
                &canonical, &target, &rel_entry, root, excluded, ancestors, report,
            );
            ancestors.pop();
            result?;
        } else if meta.is_file() {
            report.bytes += meta.len();
            if report.bytes > SNAPSHOT_MAX_BYTES {
                bail!(
                    "Dossier du playbook trop volumineux pour la copie Ansible (> {} Mio). \
                     Placez le playbook dans un sous-dossier dédié (ex. ansible/).",
                    SNAPSHOT_MAX_BYTES >> 20
                );
            }
            std::fs::copy(&canonical, &target).with_context(|| {
                format!(
                    "Impossible de copier {} vers {}",
                    canonical.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Recrée le dossier de travail du lab (jamais à travers un lien symbolique).
fn reset_workspace(ws: &Path) -> Result<()> {
    match std::fs::symlink_metadata(ws) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => std::fs::remove_file(ws)?,
        Ok(_) => std::fs::remove_dir_all(ws)
            .with_context(|| format!("Impossible de vider {}", ws.display()))?,
        Err(_) => {}
    }
    if let Some(parent) = ws.parent() {
        if !parent.is_dir() {
            create_private_dir(parent)?;
        }
    }
    Ok(())
}

/// Supprime le dossier de travail du lab. Idempotent.
fn remove_workspace(paths: &VirtumPaths, lab_name: &str) {
    let ws = workspace_path(paths, lab_name);
    if std::fs::symlink_metadata(&ws).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir_all(&ws);
    }
}

/// Joue le playbook du lab (inventaire, requirements, `ansible-playbook`) et met à jour
/// l'état. `extra_args` est transmis tel quel à `ansible-playbook`.
pub async fn run_playbook(
    lab: &Virtumfile,
    virtumfile_path: &Path,
    paths: &VirtumPaths,
    extra_args: &[String],
    insecure_ssh: bool,
) -> Result<RunOutcome> {
    let spec = lab
        .ansible
        .as_ref()
        .context("Pas de section `ansible:` dans le Virtumfile.")?;
    require_tools(spec)?;
    let vf_dir = virtumfile::virtumfile_dir(virtumfile_path);
    let playbook = virtumfile::resolve_ansible_path(&vf_dir, "playbook", &spec.playbook)?;
    let shares = writable_shares(lab, &vf_dir, paths).await?;
    virtumfile::ensure_host_code_outside_shares(lab, &vf_dir, &shares)
        .context("Provisionnement refusé")?;

    let inventory = prepare_inventory(lab, paths, insecure_ssh).await?;

    let mut state = load_state(paths, &lab.name)?.unwrap_or_default();
    state.playbook = spec.playbook.clone();
    if let Some(ref req) = spec.requirements {
        let req = virtumfile::resolve_ansible_path(&vf_dir, "requirements", req)?;
        if let Err(err) = install_requirements(&req, &vf_dir, &shares, &mut state).await {
            state.last_run = Some("failed".into());
            save_state(paths, &lab.name, &state)?;
            return Err(err);
        }
    }

    // ansible-playbook tourne sur une copie privée du dossier du playbook, sans les
    // partages inscriptibles : rien de ce qu'il charge à côté du playbook (rôles, tâches,
    // modèles, variables, plugins) ne peut avoir été écrit par un invité. Un chemin qui
    // sort de ce dossier (`../commun`) n'existe pas dans la copie : échec, pas d'exécution.
    let ws = workspace_path(paths, &lab.name);
    reset_workspace(&ws)?;
    create_private_dir(&ws)?;
    let copy = ws.join("playbook");
    let playbook_dir = playbook
        .parent()
        .context("Playbook sans dossier parent")?
        .to_path_buf();
    let excluded: Vec<PathBuf> = shares.iter().map(|(_, p)| p.clone()).collect();
    let report = snapshot_project(&playbook_dir, &copy, &excluded)?;
    if !report.skipped.is_empty() {
        let list: Vec<String> = report
            .skipped
            .iter()
            .map(|p| format!("./{}", p.display()))
            .collect();
        eprintln!(
            "  Ansible : partages inscriptibles exclus de la copie du playbook : {}",
            list.join(", ")
        );
    }
    let copied_playbook = copy.join(playbook.file_name().context("Playbook sans nom")?);

    eprintln!(
        "Ansible : {} sur le lab '{}' (inventaire {})",
        spec.playbook,
        lab.name,
        inventory.display()
    );
    let mut cmd = tokio::process::Command::new("ansible-playbook");
    cmd.arg("-i")
        .arg(&inventory)
        .arg(&copied_playbook)
        .args(extra_args)
        // ansible.cfg placé à côté du playbook est lu ; `-i` remplace son `inventory =`.
        .current_dir(copied_playbook.parent().unwrap_or(&copy));
    let code = run_inherited(cmd).await;
    remove_workspace(paths, &lab.name);
    let code = code?;

    let outcome = if code == 0 {
        state.last_run = Some("success".into());
        state.provisioned_at =
            Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        RunOutcome::Success
    } else {
        state.last_run = Some("failed".into());
        RunOutcome::Failed(code)
    };
    save_state(paths, &lab.name, &state)?;
    Ok(outcome)
}

/// `virtum provision` : rejoue le playbook ; code de sortie d'Ansible repris.
pub fn provision(file: Option<PathBuf>, args: Vec<String>, insecure_ssh: bool) -> Result<()> {
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(async {
            let path = virtumfile::resolve_virtumfile_path(file)?;
            let lab = virtumfile::load(&path)?;
            let paths = config::ensure_layout().await?;
            run_playbook(&lab, &path, &paths, &args, insecure_ssh).await
        })?;
    match outcome {
        RunOutcome::Success => {
            println!("Provisionnement réussi.");
            Ok(())
        }
        RunOutcome::Failed(code) => {
            eprintln!("Provisionnement en échec (ansible-playbook, code {code}).");
            std::process::exit(code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_kinds_detects_collections_and_roles() {
        assert_eq!(
            requirements_kinds("---\ncollections:\n  - name: ansible.posix\n"),
            (true, false)
        );
        assert_eq!(
            requirements_kinds("roles:\n  - name: geerlingguy.docker\ncollections: []\n"),
            (false, true)
        );
        assert_eq!(
            requirements_kinds("- src: geerlingguy.docker\n"),
            (false, true)
        );
        assert_eq!(requirements_kinds("---\n"), (false, false));
    }

    #[test]
    fn state_roundtrip_and_paths() {
        let root =
            std::env::temp_dir().join(format!("virtum-ansible-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = VirtumPaths {
            root: root.clone(),
            images: root.join("images"),
            instances: root.join("instances"),
            keys: root.join("keys"),
            metadata: root.join("metadata.json"),
        };
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(load_state(&paths, "k8s").unwrap(), None);
        let state = AnsibleState {
            provisioned_at: Some("2026-11-20T10:12:00Z".into()),
            last_run: Some("success".into()),
            playbook: "pb/site.yml".into(),
            requirements_sha256: None,
            snapshot: Some("provisioned".into()),
        };
        save_state(&paths, "k8s", &state).unwrap();
        assert_eq!(load_state(&paths, "k8s").unwrap(), Some(state.clone()));
        assert!(state.succeeded());
        assert!(state_path(&paths, "k8s").ends_with("ansible-k8s.json"));
        remove_state(&paths, "k8s");
        assert_eq!(load_state(&paths, "k8s").unwrap(), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn provision_reason_rules() {
        let ok = AnsibleState {
            last_run: Some("success".into()),
            ..Default::default()
        };
        let failed = AnsibleState {
            last_run: Some("failed".into()),
            provisioned_at: Some("2026-11-20T10:12:00Z".into()),
            ..Default::default()
        };
        assert_eq!(provision_reason(None, false), Some("lab non provisionné"));
        assert_eq!(
            provision_reason(Some(&failed), false),
            Some("dernier provisionnement en échec")
        );
        assert_eq!(provision_reason(Some(&ok), true), Some("machines ajoutées"));
        assert_eq!(provision_reason(Some(&ok), false), None);
    }

    /// La copie privée du playbook omet tout ce qui est dans un partage inscriptible, même
    /// atteint par un lien, et fige le reste.
    #[test]
    fn snapshot_excludes_writable_shares() {
        use std::os::unix::fs::symlink;
        let d = std::env::temp_dir().join(format!("virtum-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let pb = d.join("pb");
        for sub in ["roles/web/tasks", "roles/shared", ".git"] {
            std::fs::create_dir_all(pb.join(sub)).unwrap();
        }
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(pb.join("site.yml"), "---\n").unwrap();
        std::fs::write(pb.join("roles/web/tasks/main.yml"), "- debug: msg=ok\n").unwrap();
        std::fs::write(pb.join("roles/shared/evil.yml"), "- shell: id\n").unwrap();
        std::fs::write(pb.join(".git/config"), "").unwrap();
        std::fs::write(d.join("data/all.yml"), "x: 1\n").unwrap();
        symlink(d.join("data"), pb.join("group_vars")).unwrap();
        symlink(pb.join("roles/web"), pb.join("web_alias")).unwrap();
        symlink(d.join("outside"), pb.join("ext")).unwrap();
        symlink(&pb, pb.join("loop")).unwrap();

        let root = pb.canonicalize().unwrap();
        let excluded = vec![
            pb.join("roles/shared").canonicalize().unwrap(),
            d.join("data").canonicalize().unwrap(),
        ];
        let dest = d.join("copy");
        let report = snapshot_project(&root, &dest, &excluded).unwrap();
        let mut skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        skipped.sort();
        assert!(skipped.contains(&"group_vars".to_string()), "{skipped:?}");
        assert!(skipped.contains(&"roles/shared".to_string()), "{skipped:?}");
        assert!(dest.join("site.yml").is_file());
        assert!(dest.join("roles/web/tasks/main.yml").is_file());
        assert!(!dest.join("roles/shared").exists());
        assert!(!dest.join("group_vars").exists());
        assert!(!dest.join(".git").exists());
        // Lien interne : contenu copié ; lien externe : lien absolu vers la cible réelle.
        assert!(!std::fs::symlink_metadata(dest.join("web_alias"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(dest.join("web_alias/tasks/main.yml").is_file());
        assert_eq!(
            std::fs::read_link(dest.join("ext")).unwrap(),
            d.join("outside").canonicalize().unwrap()
        );
        // Une modification après la copie ne l'atteint pas.
        std::fs::write(pb.join("roles/web/tasks/main.yml"), "- shell: id\n").unwrap();
        assert!(
            std::fs::read_to_string(dest.join("roles/web/tasks/main.yml"))
                .unwrap()
                .contains("debug")
        );
        // Le dossier lui-même dans un partage inscriptible : refusé.
        let err = snapshot_project(&root, &d.join("copy2"), std::slice::from_ref(&root));
        assert!(err.is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    /// Audit run-4 : un lien qui sort du dossier du playbook vers un dossier contenant un
    /// partage inscriptible est refusé (il resterait actif dans la copie).
    #[test]
    fn snapshot_refuses_link_to_dir_containing_share() {
        let d = crate::test_env::private_temp_dir("snapshot-link")
            .canonicalize()
            .unwrap();
        std::fs::create_dir_all(d.join("ansible")).unwrap();
        std::fs::create_dir_all(d.join("roles/devrole/tasks")).unwrap();
        std::fs::write(d.join("ansible/site.yml"), "---\n").unwrap();
        std::os::unix::fs::symlink("../roles", d.join("ansible/roles")).unwrap();
        let root = d.join("ansible");
        let share = d.join("roles/devrole");
        let err = snapshot_project(&root, &d.join("copy"), std::slice::from_ref(&share));
        assert!(format!("{:#}", err.unwrap_err()).contains("contient le partage"));
        // Sans partage sous la cible, le lien est gardé.
        snapshot_project(&root, &d.join("copy2"), &[]).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    /// Audit run-4 : une source locale de requirements dans un partage inscriptible est
    /// refusée, quelle que soit la base de résolution.
    #[test]
    fn requirements_local_source_in_share_is_refused() {
        let d = crate::test_env::private_temp_dir("req-src")
            .canonicalize()
            .unwrap();
        std::fs::create_dir_all(d.join("out/coll")).unwrap();
        let shares = vec![("web/out".to_string(), d.join("out"))];
        let req = "collections:\n  - name: ./out/coll\n    type: dir\nroles:\n  - src: file://./out/r.tar.gz\n";
        assert_eq!(
            requirements_local_paths(req),
            ["./out/coll", "./out/r.tar.gz"]
        );
        let err = ensure_requirements_sources_outside_shares(req, &[d.clone()], &shares);
        assert!(format!("{:#}", err.unwrap_err()).contains("partage"));
        // Collection Galaxy distante : aucun chemin local, accepté.
        let ok = "collections:\n  - name: community.general\n    version: '>=8'\n";
        ensure_requirements_sources_outside_shares(ok, &[d.clone()], &shares).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn finds_commands_in_path() {
        assert!(command_in_path("sh"));
        assert!(!command_in_path("virtum-command-qui-n-existe-pas"));
    }
}
