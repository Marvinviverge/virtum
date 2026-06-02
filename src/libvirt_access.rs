//! Permissions pour que QEMU système (`libvirt-qemu`) lise les disques Virtum.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::command::{path_arg, run_command};
use crate::config::{self, VirtumPaths};

pub const LIBVIRT_GROUP: &str = "libvirt";
const LIBVIRT_QEMU_USER: &str = "libvirt-qemu";

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
            "QEMU système ne peut pas lire les fichiers sous {}.\n\
             Vérifiez : sudo chown -R \"$USER:libvirt\" {}\n\
             Puis relancez virtum run.",
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
    for dir in [&paths.root, &paths.images, &paths.instances, instance_dir] {
        set_libvirt_group_readable(dir, true)?;
    }
    for file in [base_image, disk_path, iso_path] {
        set_libvirt_group_readable(file, false)?;
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
    Ok(())
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
    set_libvirt_group_readable(path, false)
}

fn set_libvirt_group_readable(path: &Path, is_dir: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(path)
        .with_context(|| format!("Impossible de lire les métadonnées de {}", path.display()))?;
    let current_uid = unsafe { libc::getuid() };

    if meta.uid() != current_uid {
        return Ok(());
    }

    run_command("chgrp", &[LIBVIRT_GROUP, path_arg(path)?]).with_context(|| {
        format!(
            "chgrp {LIBVIRT_GROUP} a échoué sur {}. \
             Ajoutez-vous au groupe libvirt (virtum doctor).",
            path.display()
        )
    })?;

    let mode = if is_dir { 0o775 } else { 0o664 };
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
    run_command("setfacl", &["-m", &spec, path_arg(path)?]).map(|_| ())
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

/// Image de base (après téléchargement).
pub fn set_image_file_for_libvirt(path: &Path) -> Result<()> {
    if config::uses_var_lib_storage() {
        set_libvirt_group_readable(path, false)
    } else {
        prepare_data_file_for_qemu(path)
    }
}
