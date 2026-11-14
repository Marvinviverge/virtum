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
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
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

/// Installe les requirements si le fichier a changé depuis la dernière installation réussie.
async fn install_requirements(requirements: &Path, state: &mut AnsibleState) -> Result<()> {
    let content = std::fs::read(requirements)
        .with_context(|| format!("Impossible de lire {}", requirements.display()))?;
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

/// Dossiers hôte partagés par les VM déployées du lab, d'après leur `mounts-state.json`.
async fn lab_active_shares(
    lab: &Virtumfile,
    paths: &VirtumPaths,
) -> Result<Vec<(String, PathBuf)>> {
    let metadata = crate::metadata::load_metadata(&paths.metadata).await?;
    let mut shares = Vec::new();
    for inst in crate::metadata::instances_for_lab(&metadata, &lab.name) {
        let dir = crate::vm::instance_dir_for(paths, &inst.id)?;
        if !dir.is_dir() {
            continue;
        }
        for m in crate::mounts::load_mounts_state(&dir)? {
            let machine = inst.machine_name.as_deref().unwrap_or(&inst.id);
            shares.push((format!("{machine}/{}", m.name), m.host_abs));
        }
    }
    Ok(shares)
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
    // Partages réellement servis aux VM du lab (y compris ceux retirés du Virtumfile mais
    // encore actifs) : aucun ne doit recouvrir ce qu'ansible-playbook va exécuter.
    let active_shares = lab_active_shares(lab, paths).await?;
    virtumfile::ensure_host_code_outside_shares(lab, &vf_dir, &active_shares)
        .context("Provisionnement refusé")?;

    let inventory = prepare_inventory(lab, paths, insecure_ssh).await?;

    let mut state = load_state(paths, &lab.name)?.unwrap_or_default();
    state.playbook = spec.playbook.clone();
    if let Some(ref req) = spec.requirements {
        let req = virtumfile::resolve_ansible_path(&vf_dir, "requirements", req)?;
        if let Err(err) = install_requirements(&req, &mut state).await {
            state.last_run = Some("failed".into());
            save_state(paths, &lab.name, &state)?;
            return Err(err);
        }
    }

    eprintln!(
        "Ansible : {} sur le lab '{}' (inventaire {})",
        spec.playbook,
        lab.name,
        inventory.display()
    );
    let mut cmd = tokio::process::Command::new("ansible-playbook");
    cmd.arg("-i")
        .arg(&inventory)
        .arg(&playbook)
        .args(extra_args)
        // ansible.cfg placé à côté du playbook est lu ; `-i` remplace son `inventory =`.
        .current_dir(playbook.parent().unwrap_or(&vf_dir));
    let code = run_inherited(cmd).await?;

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

    #[test]
    fn finds_commands_in_path() {
        assert!(command_in_path("sh"));
        assert!(!command_in_path("virtum-command-qui-n-existe-pas"));
    }
}
