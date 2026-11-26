use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

use crate::tr;

/// Runs a program with structured arguments (no shell).
pub fn run_command(program: &str, args: &[&str]) -> Result<Output> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| tr!(fr: "Impossible d'exécuter {program}", en: "Cannot run {program}"))?;

    if output.status.success() {
        Ok(output)
    } else {
        let stderr = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stderr));
        let stdout = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stdout));
        bail!(tr!(
            fr: "{program} a échoué (code {:?}).\nstdout: {stdout}\nstderr: {stderr}",
            en: "{program} failed (code {:?}).\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        ));
    }
}

pub fn path_arg(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| {
        let path = path.display();
        tr!(
            fr: "Chemin non UTF-8 (inattendu sur Linux): {path}",
            en: "Non-UTF-8 path (unexpected on Linux): {path}"
        )
    })
}

/// Cherche un exécutable dans le `PATH`, comme `command -v`, sans dépendre de la commande
/// `which` (absente des images minimales de Fedora et d'Arch, par exemple).
pub fn find_in_path(bin: &str) -> Option<PathBuf> {
    find_in(bin, &std::env::var_os("PATH")?)
}

/// Vrai si `bin` est un exécutable du `PATH`.
pub fn command_exists(bin: &str) -> bool {
    find_in_path(bin).is_some()
}

fn find_in(bin: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    if bin.is_empty() || bin.contains('/') {
        return None;
    }
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(bin))
        .find(|p| is_executable_file(p))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Chemin à donner à l'utilisateur pour un script de préparation de l'hôte
/// (`setup-var-lib.sh`, `setup-apparmor-virtiofs.sh`) : celui du paquet `.deb` s'il est
/// installé, sinon le dossier `scripts/` de l'archive ou des sources.
pub fn host_script_hint(name: &str) -> String {
    let installed = Path::new("/usr/share/virtum").join(name);
    if installed.is_file() {
        installed.display().to_string()
    } else {
        tr!(
            fr: "<dossier de l'archive ou des sources>/scripts/{name}",
            en: "<archive or source folder>/scripts/{name}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn find_in_finds_executables_only() {
        let dir = std::env::temp_dir().join(format!("virtum-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("outil-exe");
        let plain = dir.join("outil-texte");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::write(&plain, "texte").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path: OsString = format!("relatif:/inexistant:{}", dir.display()).into();
        assert_eq!(find_in("outil-exe", &path), Some(exe.clone()));
        assert_eq!(find_in("outil-texte", &path), None);
        assert_eq!(find_in("absent", &path), None);
        assert_eq!(find_in("", &path), None);
        assert_eq!(find_in(&exe.display().to_string(), &path), None);
        // Un dossier relatif du PATH est ignoré.
        let rel: OsString = ".".into();
        assert_eq!(find_in("outil-exe", &rel), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn command_exists_finds_sh() {
        assert!(command_exists("sh"));
        assert!(!command_exists("binaire-virtum-inexistant"));
    }
}
