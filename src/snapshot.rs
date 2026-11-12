//! `virtum snapshot` — instantanés à froid des disques (qcow2 internes, `qemu-img snapshot`).
//!
//! L'état mémoire n'est pas capturé : revenir à un instantané revient à redémarrer depuis ce disque.
//! La VM est arrêtée pendant l'opération (qemu-img ne doit pas écrire sur un disque ouvert par
//! QEMU), puis relancée si elle tournait. Pour un lab, toutes les VM sont arrêtées avant le
//! premier instantané : les disques correspondent au même instant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde_json::Value;

use crate::command::{path_arg, run_command};
use crate::config::{self, VirtumPaths};
use crate::disk::{format_virtual_size_label, image_virtual_size_bytes, parse_disk_size};
use crate::metadata::{self, Instance, InstanceStatus};
use crate::virtumfile;
use crate::vm;

/// Préfixe des tags qcow2 posés par Virtum (les autres instantanés sont ignorés).
const TAG_PREFIX: &str = "virtum-";
/// En dessous, `create` avertit : les blocs réécrits après l'instantané sont conservés en double.
const LOW_FREE_SPACE_BYTES: u64 = 10 << 30;
const MAX_NAME_LEN: usize = 64;

/// Instantané Virtum présent dans un disque qcow2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSnapshot {
    pub name: String,
    /// Date de création (secondes Unix).
    pub date_sec: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiskInfo {
    snapshots: Vec<DiskSnapshot>,
    /// Place occupée sur l'hôte (instantanés compris).
    actual_size: u64,
}

/// Nom d'instantané : lettres, chiffres, tiret, underscore (comme les noms de VM).
pub fn validate_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Le nom d'instantané ne peut pas être vide.");
    }
    if name.len() > MAX_NAME_LEN {
        bail!("Le nom d'instantané dépasse {MAX_NAME_LEN} caractères.");
    }
    if name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "Le nom d'instantané '{name}' n'est pas valide (autorisé : lettres, chiffres, tiret, underscore)."
        );
    }
    Ok(())
}

fn tag_for(name: &str) -> String {
    format!("{TAG_PREFIX}{name}")
}

fn disk_info(disk_path: &Path) -> Result<DiskInfo> {
    let output = run_command(
        "qemu-img",
        &["info", "-U", "--output=json", path_arg(disk_path)?],
    )?;
    parse_disk_info(&String::from_utf8_lossy(&output.stdout))
}

fn parse_disk_info(json: &str) -> Result<DiskInfo> {
    let info: Value = serde_json::from_str(json)
        .context("Impossible de parser la sortie qemu-img info (JSON)")?;
    let snapshots = info
        .get("snapshots")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|s| {
                    let name = s.get("name")?.as_str()?.strip_prefix(TAG_PREFIX)?;
                    Some(DiskSnapshot {
                        name: name.to_string(),
                        date_sec: s.get("date-sec").and_then(Value::as_i64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let actual_size = info.get("actual-size").and_then(Value::as_u64).unwrap_or(0);
    Ok(DiskInfo {
        snapshots,
        actual_size,
    })
}

/// Instantanés Virtum d'un disque (lecture seule, VM allumée possible).
pub fn list_disk_snapshots(disk_path: &Path) -> Result<Vec<DiskSnapshot>> {
    Ok(disk_info(disk_path)?.snapshots)
}

/// Le disque ne doit pas être ouvert par QEMU (VM arrêtée).
pub fn create_disk_snapshot(disk_path: &Path, name: &str) -> Result<()> {
    run_qemu_img_snapshot("-c", disk_path, name)
}

/// Le disque ne doit pas être ouvert par QEMU (VM arrêtée).
pub fn delete_disk_snapshot(disk_path: &Path, name: &str) -> Result<()> {
    run_qemu_img_snapshot("-d", disk_path, name)
}

/// Remet le disque (contenu et taille virtuelle) dans l'état de l'instantané. VM arrêtée.
pub fn apply_disk_snapshot(disk_path: &Path, name: &str) -> Result<()> {
    run_qemu_img_snapshot("-a", disk_path, name)
}

fn run_qemu_img_snapshot(flag: &str, disk_path: &Path, name: &str) -> Result<()> {
    let tag = tag_for(name);
    run_command(
        "qemu-img",
        &["snapshot", flag, tag.as_str(), path_arg(disk_path)?],
    )
    .with_context(|| {
        format!(
            "qemu-img snapshot {flag} {tag} a échoué sur {}",
            disk_path.display()
        )
    })?;
    Ok(())
}

/// VM visée par une commande `snapshot`.
#[derive(Debug, Clone)]
struct Target {
    instance: Instance,
    disk: PathBuf,
}

impl Target {
    fn label(&self) -> &str {
        self.instance.name.as_deref().unwrap_or(&self.instance.id)
    }
}

fn labels(targets: &[&Target]) -> String {
    targets
        .iter()
        .map(|t| t.label())
        .collect::<Vec<_>>()
        .join(", ")
}

/// VM citées, sinon toutes les VM du lab du Virtumfile (`-f` ou `./Virtumfile`).
async fn resolve_targets(
    paths: &VirtumPaths,
    vms: &[String],
    file: Option<PathBuf>,
) -> Result<Vec<Target>> {
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    let instances: Vec<Instance> = if vms.is_empty() {
        let path = virtumfile::resolve_virtumfile_path(file)?;
        let lab = virtumfile::load(&path)?;
        let instances: Vec<Instance> = metadata::instances_for_lab(&metadata, &lab.name)
            .into_iter()
            .cloned()
            .collect();
        if instances.is_empty() {
            bail!(
                "Aucune VM pour le lab '{}'. Lancez `virtum up`, ou précisez les VM visées.",
                lab.name
            );
        }
        instances
    } else {
        let mut instances: Vec<Instance> = Vec::new();
        for reference in vms {
            let instance = metadata::resolve_instance_ref(&metadata, reference)?;
            if !instances.iter().any(|i| i.id == instance.id) {
                instances.push(instance.clone());
            }
        }
        instances
    };

    let mut targets = Vec::new();
    for instance in instances {
        let instance = vm::refresh_instance_status(&paths.metadata, instance).await?;
        let disk = paths.instances.join(&instance.id).join("disk.qcow2");
        if !disk.is_file() {
            bail!(
                "Disque introuvable pour '{}' : {}",
                instance.name.as_deref().unwrap_or(&instance.id),
                disk.display()
            );
        }
        targets.push(Target { instance, disk });
    }
    Ok(targets)
}

/// Arrêt propre (ACPI) avant de figer un disque, ou immédiat quand son état est abandonné.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMode {
    Graceful,
    Force,
}

/// Arrête les VM allumées, exécute `op`, puis relance celles qui tournaient (si `restart`).
///
/// La relance a lieu même si `op` échoue : la VM retrouve l'état où on l'a prise.
async fn with_vms_stopped<F>(
    targets: &[&Target],
    stop_mode: StopMode,
    restart: bool,
    op: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let running: Vec<&Target> = targets
        .iter()
        .copied()
        .filter(|t| t.instance.status == InstanceStatus::Running)
        .collect();

    let mut stopped: Vec<&Target> = Vec::new();
    let mut result = Ok(());
    for target in &running {
        eprintln!("Arrêt de '{}'…", target.label());
        let stop = match stop_mode {
            StopMode::Graceful => vm::stop_vm_async(target.label()).await,
            StopMode::Force => vm::kill_vm_async(target.label()).await,
        };
        if let Err(err) = stop {
            result = Err(err.context(format!("Arrêt de '{}' impossible", target.label())));
            break;
        }
        stopped.push(target);
    }
    if result.is_ok() {
        result = op();
    }

    if !restart {
        if !stopped.is_empty() {
            eprintln!("VM laissées arrêtées (--no-restart) : {}", labels(&stopped));
        }
        return result;
    }
    let mut restart_failures = Vec::new();
    for target in &stopped {
        eprintln!("Relance de '{}'…", target.label());
        if let Err(err) = vm::start_vm_async(target.label()).await {
            eprintln!("error [{}]: {err:#}", target.label());
            restart_failures.push(target.label().to_string());
        }
    }
    result?;
    if !restart_failures.is_empty() {
        bail!(
            "Opération faite, mais relance impossible pour : {}. Relancez avec `virtum start`.",
            restart_failures.join(", ")
        );
    }
    Ok(())
}

fn warn_low_free_space(dir: &Path) {
    if let Some(free) = free_space_bytes(dir) {
        if free < LOW_FREE_SPACE_BYTES {
            eprintln!(
                "Avertissement: seulement {} libres sous {} — chaque bloc réécrit après \
                 l'instantané y sera conservé en double.",
                format_bytes(free),
                dir.display()
            );
        }
    }
}

fn free_space_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail * stat.f_frsize)
}

fn warn_mounts_not_included(targets: &[Target]) {
    for target in targets {
        if target
            .instance
            .mounts
            .as_ref()
            .is_some_and(|m| !m.is_empty())
        {
            eprintln!(
                "Avertissement: les dossiers partagés (`mounts:`) de '{}' restent sur l'hôte \
                 et ne font pas partie de l'instantané.",
                target.label()
            );
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1u64 << 30) as f64)
}

fn block_on<F: std::future::Future<Output = Result<()>>>(future: F) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(future)
}

pub fn create(name: &str, vms: &[String], file: Option<PathBuf>, no_restart: bool) -> Result<()> {
    block_on(create_async(name, vms, file, no_restart))
}

pub(crate) async fn create_async(
    name: &str,
    vms: &[String],
    file: Option<PathBuf>,
    no_restart: bool,
) -> Result<()> {
    validate_snapshot_name(name)?;
    let paths = config::ensure_layout().await?;
    let targets = resolve_targets(&paths, vms, file).await?;

    // qemu-img accepte deux tags identiques : on refuse avant de toucher aux VM.
    for target in &targets {
        if list_disk_snapshots(&target.disk)?
            .iter()
            .any(|s| s.name == name)
        {
            bail!(
                "L'instantané '{name}' existe déjà sur '{label}'. Supprimez-le d'abord : `virtum snapshot rm {name} {label}`.",
                label = target.label()
            );
        }
    }
    warn_low_free_space(&paths.instances);
    warn_mounts_not_included(&targets);

    let refs: Vec<&Target> = targets.iter().collect();
    with_vms_stopped(&refs, StopMode::Graceful, !no_restart, || {
        let mut done: Vec<&Target> = Vec::new();
        for target in &targets {
            if let Err(err) = create_disk_snapshot(&target.disk, name) {
                for created in &done {
                    let _ = delete_disk_snapshot(&created.disk, name);
                }
                return Err(err.context(format!(
                    "Instantané de '{}' impossible ; instantanés partiels annulés.",
                    target.label()
                )));
            }
            eprintln!("  Instantané '{name}' pris pour '{}'.", target.label());
            done.push(target);
        }
        Ok(())
    })
    .await?;

    println!("Instantané '{name}' créé : {}", labels(&refs));
    Ok(())
}

pub fn restore(name: &str, vms: &[String], file: Option<PathBuf>, no_restart: bool) -> Result<()> {
    block_on(restore_async(name, vms, file, no_restart))
}

async fn restore_async(
    name: &str,
    vms: &[String],
    file: Option<PathBuf>,
    no_restart: bool,
) -> Result<()> {
    validate_snapshot_name(name)?;
    let paths = config::ensure_layout().await?;
    let targets = resolve_targets(&paths, vms, file).await?;

    // Lab cohérent : toutes les VM visées doivent porter l'instantané, sinon rien n'est touché.
    let mut missing: Vec<&Target> = Vec::new();
    for target in &targets {
        if !list_disk_snapshots(&target.disk)?
            .iter()
            .any(|s| s.name == name)
        {
            missing.push(target);
        }
    }
    if !missing.is_empty() {
        bail!(
            "Instantané '{name}' absent de : {}. Rien n'a été modifié. Voir `virtum snapshot ls`.",
            labels(&missing)
        );
    }
    warn_mounts_not_included(&targets);

    let refs: Vec<&Target> = targets.iter().collect();
    // Taille lue VM encore arrêtée : une fois relancée, QEMU verrouille le disque.
    let mut restored: Vec<(&Target, u64)> = Vec::new();
    let result = with_vms_stopped(&refs, StopMode::Force, !no_restart, || {
        for target in &targets {
            if let Err(err) = apply_disk_snapshot(&target.disk, name) {
                let done: Vec<&Target> = restored.iter().map(|(t, _)| *t).collect();
                let done = if done.is_empty() {
                    "aucune".to_string()
                } else {
                    labels(&done)
                };
                return Err(err.context(format!(
                    "Retour à '{name}' impossible pour '{}'. VM déjà restaurées : {done}.",
                    target.label()
                )));
            }
            eprintln!("  '{}' revenue à l'instantané '{name}'.", target.label());
            restored.push((target, image_virtual_size_bytes(&target.disk)?));
        }
        Ok(())
    })
    .await;
    sync_disk_sizes(&paths, &restored).await?;
    result?;

    println!("Instantané '{name}' restauré : {}", labels(&refs));
    Ok(())
}

/// Le retour à un instantané remet aussi l'ancienne taille de disque : on aligne `disk_size`.
async fn sync_disk_sizes(paths: &VirtumPaths, restored: &[(&Target, u64)]) -> Result<()> {
    for &(target, actual) in restored {
        let Some(new_label) =
            disk_size_label_after_restore(target.instance.disk_size.as_deref(), actual)
        else {
            continue;
        };
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        let Some(instance) = metadata::find_instance(&metadata, &target.instance.id) else {
            continue;
        };
        eprintln!(
            "Avertissement: le disque de '{}' est revenu à {new_label} (au lieu de {}) ; \
             `virtum plan` signalera l'écart avec le Virtumfile.",
            target.label(),
            instance.disk_size.as_deref().unwrap_or("-")
        );
        let mut instance = instance.clone();
        instance.disk_size = Some(new_label);
        metadata::upsert_instance(&paths.metadata, instance).await?;
    }
    Ok(())
}

/// Nouveau libellé `disk_size` si la taille réelle ne correspond plus au libellé enregistré.
fn disk_size_label_after_restore(stored: Option<&str>, actual_bytes: u64) -> Option<String> {
    let stored_bytes = parse_disk_size(stored?).ok()?.bytes;
    (stored_bytes != actual_bytes).then(|| format_virtual_size_label(actual_bytes))
}

pub fn remove(name: &str, vms: &[String], file: Option<PathBuf>, no_restart: bool) -> Result<()> {
    block_on(remove_async(name, vms, file, no_restart))
}

async fn remove_async(
    name: &str,
    vms: &[String],
    file: Option<PathBuf>,
    no_restart: bool,
) -> Result<()> {
    validate_snapshot_name(name)?;
    let paths = config::ensure_layout().await?;
    let targets = resolve_targets(&paths, vms, file).await?;

    let mut holders: Vec<&Target> = Vec::new();
    for target in &targets {
        if list_disk_snapshots(&target.disk)?
            .iter()
            .any(|s| s.name == name)
        {
            holders.push(target);
        }
    }
    if holders.is_empty() {
        bail!(
            "Instantané '{name}' introuvable sur {}. Voir `virtum snapshot ls`.",
            labels(&targets.iter().collect::<Vec<_>>())
        );
    }

    with_vms_stopped(&holders, StopMode::Graceful, !no_restart, || {
        for target in &holders {
            delete_disk_snapshot(&target.disk, name)?;
            eprintln!("  Instantané '{name}' supprimé de '{}'.", target.label());
        }
        Ok(())
    })
    .await?;

    println!("Instantané '{name}' supprimé : {}", labels(&holders));
    Ok(())
}

pub fn list(vms: &[String], file: Option<PathBuf>) -> Result<()> {
    block_on(list_async(vms, file))
}

async fn list_async(vms: &[String], file: Option<PathBuf>) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let targets = resolve_targets(&paths, vms, file).await?;

    let mut per_disk = Vec::new();
    for target in &targets {
        per_disk.push((target.label().to_string(), disk_info(&target.disk)?));
    }
    print!("{}", format_listing(&per_disk));
    Ok(())
}

/// Un instantané pris sur plusieurs VM (même nom) apparaît sur une seule ligne.
fn format_listing(per_disk: &[(String, DiskInfo)]) -> String {
    let mut by_name: BTreeMap<&str, (i64, Vec<&str>)> = BTreeMap::new();
    for (label, info) in per_disk {
        for snapshot in &info.snapshots {
            let entry = by_name
                .entry(snapshot.name.as_str())
                .or_insert((snapshot.date_sec, Vec::new()));
            entry.0 = entry.0.min(snapshot.date_sec);
            entry.1.push(label);
        }
    }

    let mut out = String::new();
    if by_name.is_empty() {
        let all: Vec<&str> = per_disk.iter().map(|(l, _)| l.as_str()).collect();
        out.push_str(&format!("Aucun instantané ({}).\n", all.join(", ")));
    } else {
        let mut rows: Vec<(&str, i64, Vec<&str>)> = by_name
            .into_iter()
            .map(|(name, (date, vms))| (name, date, vms))
            .collect();
        rows.sort_by_key(|(name, date, _)| (*date, *name));
        out.push_str(&format!("{:<24} {:<17} {}\n", "SNAPSHOT", "DATE", "VMS"));
        for (name, date, vms) in rows {
            out.push_str(&format!(
                "{:<24} {:<17} {}\n",
                name,
                format_date(date),
                vms.join(", ")
            ));
        }
    }
    let sizes: Vec<String> = per_disk
        .iter()
        .map(|(label, info)| format!("{label} {}", format_bytes(info.actual_size)))
        .collect();
    out.push_str(&format!(
        "Disques (instantanés compris) : {}\n",
        sizes.join(", ")
    ));
    out
}

fn format_date(date_sec: i64) -> String {
    DateTime::from_timestamp(date_sec, 0)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_size_label_follows_restored_disk() {
        // apply a agrandi à 30G après l'instantané pris à 20G.
        assert_eq!(
            disk_size_label_after_restore(Some("30G"), 20 << 30),
            Some("20G".to_string())
        );
        assert_eq!(disk_size_label_after_restore(Some("20G"), 20 << 30), None);
        assert_eq!(disk_size_label_after_restore(None, 20 << 30), None);
    }

    #[test]
    fn validate_snapshot_name_rules() {
        assert!(validate_snapshot_name("avant-tls_2").is_ok());
        assert!(validate_snapshot_name("").is_err());
        assert!(validate_snapshot_name("-x").is_err());
        assert!(validate_snapshot_name("a b").is_err());
        assert!(validate_snapshot_name("a/b").is_err());
        assert!(validate_snapshot_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn parse_disk_info_keeps_only_virtum_tags() {
        let json = r#"{
            "actual-size": 6241124352,
            "snapshots": [
                {"id": "1", "name": "virtum-avant-tls", "date-sec": 1793260800},
                {"id": "2", "name": "manuel", "date-sec": 1793260900}
            ]
        }"#;
        let info = parse_disk_info(json).unwrap();
        assert_eq!(
            info.snapshots,
            vec![DiskSnapshot {
                name: "avant-tls".into(),
                date_sec: 1793260800
            }]
        );
        assert_eq!(info.actual_size, 6241124352);
        assert!(parse_disk_info(r#"{"actual-size": 1}"#)
            .unwrap()
            .snapshots
            .is_empty());
    }

    #[test]
    fn format_listing_groups_vms_by_snapshot() {
        let snap = |name: &str, date_sec| DiskSnapshot {
            name: name.into(),
            date_sec,
        };
        let per_disk = vec![
            (
                "app".to_string(),
                DiskInfo {
                    snapshots: vec![snap("base", 100), snap("avant-tls", 200)],
                    actual_size: 3 << 30,
                },
            ),
            (
                "edge".to_string(),
                DiskInfo {
                    snapshots: vec![snap("base", 101)],
                    actual_size: 1 << 30,
                },
            ),
        ];
        let out = format_listing(&per_disk);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("SNAPSHOT"));
        assert!(lines[1].starts_with("base") && lines[1].ends_with("app, edge"));
        assert!(lines[2].starts_with("avant-tls") && lines[2].ends_with("app"));
        assert_eq!(
            lines[3],
            "Disques (instantanés compris) : app 3.0G, edge 1.0G"
        );

        let empty = format_listing(&[(
            "app".to_string(),
            DiskInfo {
                snapshots: vec![],
                actual_size: 0,
            },
        )]);
        assert!(empty.starts_with("Aucun instantané (app)."));
    }

    #[test]
    fn disk_snapshot_create_list_delete_with_qemu_img() {
        let dir = std::env::temp_dir().join(format!("virtum-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.qcow2");
        run_command(
            "qemu-img",
            &[
                "create",
                "-q",
                "-f",
                "qcow2",
                path_arg(&disk).unwrap(),
                "64M",
            ],
        )
        .unwrap();

        create_disk_snapshot(&disk, "base").unwrap();
        let listed = list_disk_snapshots(&disk).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "base");
        assert!(listed[0].date_sec > 0);

        // Le retour à l'instantané remet aussi la taille d'avant un resize.
        run_command(
            "qemu-img",
            &["resize", "-q", path_arg(&disk).unwrap(), "128M"],
        )
        .unwrap();
        apply_disk_snapshot(&disk, "base").unwrap();
        assert_eq!(image_virtual_size_bytes(&disk).unwrap(), 64 << 20);

        delete_disk_snapshot(&disk, "base").unwrap();
        assert!(list_disk_snapshots(&disk).unwrap().is_empty());
        assert!(delete_disk_snapshot(&disk, "base").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
