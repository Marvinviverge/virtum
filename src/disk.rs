//! Copy-on-Write instance disks via qemu-img (step 10).

use std::path::Path;

use anyhow::{Context, Result};

use crate::command::{path_arg, run_command};

/// Creates `disk.qcow2` as a COW overlay on the read-only base cloud image.
pub fn create_cow_disk(base_image: &Path, disk_path: &Path) -> Result<()> {
    let parent = disk_path
        .parent()
        .with_context(|| format!("Répertoire parent absent pour {}", disk_path.display()))?;
    if !parent.is_dir() {
        anyhow::bail!(
            "Le répertoire d'instance n'existe pas: {}. \
             Créez-le avant d'appeler create_cow_disk.",
            parent.display()
        );
    }

    if disk_path.exists() {
        anyhow::bail!("Le disque instance existe déjà: {}", disk_path.display());
    }

    run_command(
        "qemu-img",
        &[
            "create",
            "-f",
            "qcow2",
            "-F",
            "qcow2",
            "-b",
            path_arg(base_image)?,
            path_arg(disk_path)?,
        ],
    )
    .with_context(|| {
        format!(
            "Impossible de créer le disque COW {} depuis {}",
            disk_path.display(),
            base_image.display()
        )
    })?;

    Ok(())
}

/// Returns true if `qemu-img info` reports the expected backing file.
pub fn cow_disk_has_backing_file(disk_path: &Path, base_image: &Path) -> Result<bool> {
    let output = run_command("qemu-img", &["info", path_arg(disk_path)?])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let base = base_image.to_string_lossy();
    Ok(stdout.contains(&format!("backing file: {base}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn create_cow_disk_requires_parent_directory() {
        let dir = std::env::temp_dir().join(format!("virtum-disk-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let base = dir.join("base.qcow2");
        let disk = dir.join("missing-parent").join("disk.qcow2");
        let err = create_cow_disk(&base, &disk).unwrap_err();
        assert!(err.to_string().contains("n'existe pas"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_cow_disk_with_qemu_img() {
        if !qemu_img_available() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("virtum-disk-cow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let base = dir.join("base.qcow2");
        let disk = dir.join("disk.qcow2");
        run_command(
            "qemu-img",
            &["create", "-f", "qcow2", path_arg(&base).unwrap(), "16M"],
        )
        .unwrap();
        create_cow_disk(&base, &disk).unwrap();
        assert!(cow_disk_has_backing_file(&disk, &base).unwrap());

        let _ = fs::remove_dir_all(&dir);
    }

    fn qemu_img_available() -> bool {
        std::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
