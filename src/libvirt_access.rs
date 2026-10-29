//! Permissions pour que QEMU système (`libvirt-qemu`) lise les disques Virtum.

use std::fs::{self, Permissions};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

use crate::command::{path_arg, run_command};
use crate::config::{self, VirtumPaths};

pub const LIBVIRT_GROUP: &str = "libvirt";
/// Groupe partagé par l'utilisateur Virtum et `libvirt-qemu` (sockets virtiofs).
pub const KVM_GROUP: &str = "kvm";
const LIBVIRT_QEMU_USER: &str = "libvirt-qemu";
/// Image de base partagée : lisible par tous, modifiable par personne.
const BASE_IMAGE_MODE: u32 = 0o444;

/// Prépare arborescence + disques avant `virsh start` (réseau `default`, QEMU système).
pub fn prepare_storage_for_system_qemu(
    paths: &VirtumPaths,
    base_image: &Path,
    instance_dir: &Path,
    disk_path: &Path,
    iso_path: &Path,
) -> Result<()> {
    if config::uses_var_lib_storage() {
        prepare_var_lib_storage(paths, base_image, instance_dir, disk_path, iso_path)
    } else {
        prepare_home_storage(paths, base_image, instance_dir, disk_path, iso_path)
    }
}

/// Message d'aide si `virsh start` échoue sur Permission denied.
pub fn permission_denied_hint(paths: &VirtumPaths) -> String {
    if config::uses_var_lib_storage() {
        return format!(
            "QEMU système (libvirt-qemu) ne peut pas accéder aux fichiers ou sockets virtiofs sous {}.\n\
             Sur Ubuntu/Debian, AppArmor bloque souvent les sockets virtiofsd externes :\n\
               sudo {}/scripts/setup-apparmor-virtiofs.sh\n\
             Puis vérifiez les groupes libvirt et kvm (virtum doctor). Si besoin :\n\
               sudo chown -R \"$USER:libvirt\" {}\n\
               sudo chgrp -R kvm {}/instances\n\
             Relancez ensuite virtum up.",
            paths.root.display(),
            env!("CARGO_MANIFEST_DIR"),
            paths.root.display(),
            paths.root.display()
        );
    }

    format!(
        "QEMU système (libvirt-qemu) ne peut pas lire les disques sous {} (AppArmor bloque souvent $HOME).\n\
         Solution recommandée (une fois) :\n\
           sudo {}/scripts/setup-var-lib.sh\n\
         Puis relancez virtum run (stockage dans /var/lib/virtum).\n\
         Alternative : setfacl sur $HOME + chown de l'image — voir README.",
        paths.root.display(),
        env!("CARGO_MANIFEST_DIR")
    )
}

fn prepare_var_lib_storage(
    paths: &VirtumPaths,
    base_image: &Path,
    instance_dir: &Path,
    disk_path: &Path,
    iso_path: &Path,
) -> Result<()> {
    for dir in [&paths.root, &paths.images, &paths.instances] {
        set_libvirt_group_readable(dir, true)?;
    }
    for file in [base_image, disk_path, iso_path] {
        set_libvirt_group_readable(file, false)?;
    }
    set_base_image_read_only(base_image)?;
    // Ne pas passer instance_dir en groupe « libvirt » : libvirt-qemu n'y est pas.
    if config::uses_var_lib_storage() {
        ensure_qemu_can_traverse(&paths.root)?;
        ensure_qemu_can_traverse(&paths.instances)?;
    }
    prepare_instance_dir_for_virtiofs(instance_dir)?;
    refresh_virtiofs_sockets_in_dir(instance_dir)?;
    Ok(())
}

/// Réapplique kvm + ACL sur les sockets après d'autres chmod/chgrp.
pub fn refresh_virtiofs_sockets_in_dir(instance_dir: &Path) -> Result<()> {
    prepare_instance_dir_for_virtiofs(instance_dir)?;
    let Ok(entries) = std::fs::read_dir(instance_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path_is_socket(&path) {
            prepare_virtiofs_socket(&path)?;
        }
    }
    Ok(())
}

fn prepare_home_storage(
    paths: &VirtumPaths,
    base_image: &Path,
    instance_dir: &Path,
    disk_path: &Path,
    iso_path: &Path,
) -> Result<()> {
    let home = paths
        .root
        .parent()
        .context("~/.virtum sans répertoire parent")?;

    ensure_others_traverse(home)?;
    ensure_others_traverse(&paths.root)?;
    ensure_others_traverse(&paths.images)?;
    ensure_others_traverse(&paths.instances)?;
    ensure_others_traverse(instance_dir)?;

    grant_qemu_acl(home, true)?;
    grant_qemu_acl(&paths.root, true)?;
    grant_qemu_acl(&paths.images, true)?;
    grant_qemu_acl(&paths.instances, true)?;
    grant_qemu_acl(instance_dir, true)?;
    set_default_qemu_acl(&paths.images)?;
    set_default_qemu_acl(&paths.instances)?;
    set_default_qemu_acl(instance_dir)?;

    for file in [base_image, disk_path, iso_path] {
        prepare_data_file_for_qemu(file)?;
    }
    set_base_image_read_only(base_image)
}

fn prepare_data_file_for_qemu(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(path)
        .with_context(|| format!("Impossible de lire les métadonnées de {}", path.display()))?;
    let current_uid = unsafe { libc::getuid() };

    if meta.uid() != current_uid {
        anyhow::bail!(
            "Le fichier {} appartient à uid {} (souvent libvirt-qemu après un run libvirt). \
             Repassez-le sous votre utilisateur :\n\
               sudo chown \"$USER:{LIBVIRT_GROUP}\" {}\n\
               chmod 664 {}\n\
             Ou migrez vers /var/lib/virtum : sudo ./scripts/setup-var-lib.sh",
            path.display(),
            meta.uid(),
            path.display(),
            path.display()
        );
    }

    let _ = grant_qemu_acl(path, false);
    set_group_access(path, LIBVIRT_GROUP, false, false)
}

fn set_libvirt_group_readable(path: &Path, is_dir: bool) -> Result<()> {
    set_group_access(path, LIBVIRT_GROUP, is_dir, false)
}

fn set_group_access(path: &Path, group: &str, is_dir: bool, is_socket: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(path)
        .with_context(|| format!("Impossible de lire les métadonnées de {}", path.display()))?;
    let current_uid = unsafe { libc::getuid() };

    if meta.uid() != current_uid {
        return Ok(());
    }

    run_command("chgrp", &[group, path_arg(path)?]).with_context(|| {
        format!(
            "chgrp {group} a échoué sur {}. \
             Ajoutez-vous aux groupes libvirt et kvm (virtum doctor).",
            path.display()
        )
    })?;

    let mode = if is_dir {
        0o775
    } else if is_socket {
        0o660
    } else {
        0o664
    };
    let mut perms = meta.permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Impossible de chmod {mode:o} sur {}", path.display()))?;
    Ok(())
}

fn grant_qemu_acl(path: &Path, is_dir: bool) -> Result<()> {
    if !setfacl_available() {
        return Ok(());
    }
    let spec = if is_dir {
        format!("u:{LIBVIRT_QEMU_USER}:rx")
    } else {
        format!("u:{LIBVIRT_QEMU_USER}:r")
    };
    run_command("setfacl", &["-m", &spec, path_arg(path)?]).with_context(|| {
        format!(
            "setfacl {spec} a échoué sur {}. \
             libvirt-qemu doit traverser ce chemin pour virtiofs.",
            path.display()
        )
    })?;
    Ok(())
}

/// ACL lecture+écriture sur un socket vhost-user (connexion QEMU).
fn grant_qemu_socket_acl(path: &Path) -> Result<()> {
    if !setfacl_available() {
        return Ok(());
    }
    let spec = format!("u:{LIBVIRT_QEMU_USER}:rw");
    run_command("setfacl", &["-m", &spec, path_arg(path)?]).with_context(|| {
        format!(
            "setfacl {spec} a échoué sur {}. \
             QEMU (libvirt-qemu) ne pourra pas se connecter au socket virtiofs.",
            path.display()
        )
    })?;
    Ok(())
}

/// libvirt-qemu n'est pas dans le groupe « libvirt » : traversée + ACL sur les répertoires.
fn ensure_qemu_can_traverse(dir: &Path) -> Result<()> {
    ensure_others_traverse(dir)?;
    grant_qemu_acl(dir, true)
}

fn set_default_qemu_acl(dir: &Path) -> Result<()> {
    if !setfacl_available() {
        return Ok(());
    }
    let spec = format!("u:{LIBVIRT_QEMU_USER}:rx");
    run_command("setfacl", &["-d", "-m", &spec, path_arg(dir)?]).map(|_| ())
}

fn setfacl_available() -> bool {
    std::process::Command::new("setfacl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ensure_others_traverse(dir: &Path) -> Result<()> {
    let meta = fs::metadata(dir)
        .with_context(|| format!("Impossible de lire les permissions de {}", dir.display()))?;
    if !meta.is_dir() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o001 == 0 {
        fs::set_permissions(dir, Permissions::from_mode(mode | 0o001)).with_context(|| {
            format!(
                "Impossible d'ajouter le bit traversée (o+x) sur {}.",
                dir.display()
            )
        })?;
    }
    Ok(())
}

/// Répertoire instance avant virtiofsd : QEMU système doit atteindre les sockets.
pub fn prepare_instance_dir_for_virtiofs(instance_dir: &Path) -> Result<()> {
    if config::uses_var_lib_storage() {
        // libvirt-qemu n'est pas dans le groupe « libvirt », seulement « kvm ».
        if let Some(instances) = instance_dir.parent() {
            ensure_qemu_can_traverse(instances)?;
            if let Some(root) = instances.parent() {
                ensure_qemu_can_traverse(root)?;
            }
        }
        set_group_access(instance_dir, KVM_GROUP, true, false)?;
        ensure_qemu_can_traverse(instance_dir)?;
        return Ok(());
    }
    if instance_dir.is_dir() {
        ensure_others_traverse(instance_dir)?;
        let _ = grant_qemu_acl(instance_dir, true);
    }
    Ok(())
}

/// Socket vhost-user (0660, groupe kvm + ACL libvirt-qemu).
pub fn prepare_virtiofs_socket(socket_path: &Path) -> Result<()> {
    if !path_is_socket(socket_path) {
        return Ok(());
    }
    set_group_access(socket_path, KVM_GROUP, false, true)?;
    grant_qemu_socket_acl(socket_path)
}

fn path_is_socket(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// Image de base (après téléchargement).
pub fn set_image_file_for_libvirt(path: &Path) -> Result<()> {
    if config::uses_var_lib_storage() {
        set_libvirt_group_readable(path, false)?;
    } else {
        prepare_data_file_for_qemu(path)?;
    }
    set_base_image_read_only(path)
}

/// Image de base en lecture seule (0444) : les disques COW des VM en dépendent ;
/// une écriture ou un remplacement accidentel les rendrait incohérents.
fn set_base_image_read_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(path)
        .with_context(|| format!("Impossible de lire les métadonnées de {}", path.display()))?;
    let current_uid = unsafe { libc::getuid() };
    if meta.uid() != current_uid {
        return Ok(());
    }
    let mut perms = meta.permissions();
    perms.set_mode(BASE_IMAGE_MODE);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Impossible de chmod 444 sur {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_image_becomes_read_only() {
        let dir = std::env::temp_dir().join(format!("virtum-ro-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let image = dir.join("base.qcow2");
        fs::write(&image, b"qcow2").unwrap();
        fs::set_permissions(&image, Permissions::from_mode(0o664)).unwrap();

        set_base_image_read_only(&image).unwrap();
        let mode = fs::metadata(&image).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
        // Idempotent.
        set_base_image_read_only(&image).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }
}
