use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::bridge_network;
use crate::config;
use crate::disk;
use crate::libvirt;
use crate::metadata::{self, Instance, InstanceStatus};
use crate::vm;

const KVM_DEVICE: &str = "/dev/kvm";

struct CheckResult {
    name: String,
    ok: bool,
    detail: String,
}

pub fn run() -> i32 {
    let paths = match config::ensure_layout_blocking() {
        Ok(paths) => paths,
        Err(err) => {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "Virtum doctor\n");
            let _ = writeln!(stdout, "  [FAIL] Arborescence Virtum — {err:#}");
            let _ = writeln!(
                stdout,
                "\nRésumé: impossible d'initialiser le répertoire Virtum."
            );
            return 1;
        }
    };

    let results = vec![
        check_binary(
            "qemu-img",
            &["--version"],
            "Installez qemu-utils (ex. sudo apt install qemu-utils).",
        ),
        check_binary(
            "virsh",
            &["--version"],
            "Installez libvirt-clients (ex. sudo apt install libvirt-clients).",
        ),
        check_binary(
            "cloud-localds",
            &["--help"],
            "Installez cloud-image-utils (ex. sudo apt install cloud-image-utils).",
        ),
        check_binary(
            "ssh",
            &["-V"],
            "Installez openssh-client (ex. sudo apt install openssh-client).",
        ),
        check_binary(
            "socat",
            &["-V"],
            "Requis pour `ports:` dans le Virtumfile (ex. sudo apt install socat).",
        ),
        check_virtiofsd(),
        check_apparmor_virtiofs(),
        check_kvm(),
        check_groups(),
        check_libvirt_default_network(),
        check_bridge_lan(),
        check_virtum_storage(&paths),
        check_vm_base_images(&paths),
        check_orphans(&paths),
        check_ansible(),
    ];

    print_report(&results)
}

fn check_binary(name: &str, version_args: &[&str], install_hint: &str) -> CheckResult {
    match Command::new(name).args(version_args).output() {
        Ok(output) if output.status.success() => CheckResult {
            name: name.to_string(),
            ok: true,
            detail: String::new(),
        },
        Ok(_) => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: format!("{name} est présent mais a échoué ({install_hint})"),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: format!("{name} introuvable dans le PATH. {install_hint}"),
        },
        Err(e) => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: format!("Impossible d'exécuter {name}: {e}"),
        },
    }
}

/// Ansible est optionnel (section `ansible:`, v0.8) : jamais bloquant.
fn check_ansible() -> CheckResult {
    let detail = match Command::new("ansible-playbook").arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("présent")
            .trim()
            .to_string(),
        _ => "absent (optionnel : requis seulement pour la section `ansible:` du Virtumfile ; \
              ex. sudo apt install ansible-core)"
            .to_string(),
    };
    CheckResult {
        name: "ansible-playbook".to_string(),
        ok: true,
        detail,
    }
}

fn check_virtiofsd() -> CheckResult {
    const NAME: &str = "virtiofsd";
    let Some(path) = crate::mounts::resolve_virtiofsd_path() else {
        return CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "{NAME} introuvable. Installez le paquet (sudo apt install virtiofsd). \
                 Sur Debian/Ubuntu le binaire est souvent dans /usr/libexec/virtiofsd, \
                 pas dans le PATH — Virtum le détecte automatiquement une fois installé."
            ),
        };
    };
    match Command::new(&path).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let mut detail = String::new();
            if !std::path::Path::new("/usr/bin/newuidmap").exists() {
                detail = "Paquet uidmap absent : Virtum lance virtiofsd avec --sandbox=none \
                          (isolation réduite — installez uidmap pour le sandbox virtiofsd)."
                    .to_string();
            }
            CheckResult {
                name: format!("{NAME} ({})", path.display()),
                ok: true,
                detail,
            }
        }
        Ok(_) => CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "{} est présent ({}) mais `--version` a échoué.",
                NAME,
                path.display()
            ),
        },
        Err(e) => CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!("Impossible d'exécuter {} : {e}", path.display()),
        },
    }
}

fn check_apparmor_virtiofs() -> CheckResult {
    const NAME: &str = "AppArmor virtiofs (Virtum)";
    if !config::uses_var_lib_storage() {
        return CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: "Stockage sous ~/.virtum — règle dédiée non requise.".to_string(),
        };
    }
    let snippet =
        std::path::Path::new("/etc/apparmor.d/abstractions/libvirt-qemu.d/virtum-virtiofs");
    if !snippet.is_file() {
        return CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "Les sockets virtiofsd sous /var/lib/virtum sont bloqués par AppArmor \
                 tant que la règle n'est pas installée :\n\
                   sudo {}/scripts/setup-apparmor-virtiofs.sh",
                env!("CARGO_MANIFEST_DIR")
            ),
        };
    }
    let content = std::fs::read_to_string(snippet).unwrap_or_default();
    if content.contains("/var/lib/virtum/instances") && content.contains("virtiofs-") {
        CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: snippet.display().to_string(),
        }
    } else {
        CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "{} existe mais ne contient pas la règle Virtum attendue. \
                 Réinstallez : sudo {}/scripts/setup-apparmor-virtiofs.sh",
                snippet.display(),
                env!("CARGO_MANIFEST_DIR")
            ),
        }
    }
}

fn check_kvm() -> CheckResult {
    const NAME: &str = "KVM (/dev/kvm)";
    match OpenOptions::new().read(true).write(true).open(KVM_DEVICE) {
        Ok(_) => CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: String::new(),
        },
        Err(e) => CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "Accès lecture/écriture à {KVM_DEVICE} impossible ({e}). \
                 Vérifiez que la virtualisation KVM est activée dans le BIOS/UEFI \
                 et que votre utilisateur est dans le groupe kvm \
                 (sudo usermod -aG kvm $USER, puis reconnectez la session)."
            ),
        },
    }
}

fn check_groups() -> CheckResult {
    const NAME: &str = "Groupes libvirt et kvm";
    let groups = match user_groups() {
        Ok(g) => g,
        Err(e) => {
            return CheckResult {
                name: NAME.to_string(),
                ok: false,
                detail: format!("Impossible de lire les groupes de l'utilisateur: {e}"),
            };
        }
    };

    let mut missing = Vec::new();
    if !groups.iter().any(|g| g == "libvirt") {
        missing.push("libvirt");
    }
    if !groups.iter().any(|g| g == "kvm") {
        missing.push("kvm");
    }

    if missing.is_empty() {
        CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: String::new(),
        }
    } else {
        CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "Groupes manquants: {}. Ajoutez-vous avec \
                 sudo usermod -aG {} $USER puis reconnectez votre session.",
                missing.join(", "),
                missing.join(",")
            ),
        }
    }
}

fn user_groups() -> io::Result<Vec<String>> {
    let output = Command::new("id").arg("-Gn").output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "id -Gn a échoué: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect())
}

fn check_virtum_storage(paths: &config::VirtumPaths) -> CheckResult {
    const NAME: &str = "Stockage Virtum (libvirt système)";
    if config::uses_var_lib_storage() {
        return CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: format!("Répertoire actif : {}", paths.root.display()),
        };
    }

    CheckResult {
        name: NAME.to_string(),
        ok: false,
        detail: format!(
            "Stockage sous {} — QEMU système ne peut pas lire $HOME/.virtum (AppArmor). \
             Exécutez une fois : sudo ./scripts/setup-var-lib.sh \
             (crée {}/ et copie l'image si besoin).",
            paths.root.display(),
            config::VAR_LIB_VIRTUM
        ),
    }
}

fn check_vm_base_images(paths: &config::VirtumPaths) -> CheckResult {
    let metadata = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(metadata::load_metadata(&paths.metadata)));
    match metadata {
        Ok(metadata) => base_images_result(&metadata.instances, &paths.instances),
        Err(err) => CheckResult {
            name: BASE_IMAGES_CHECK.to_string(),
            ok: false,
            detail: format!("{err:#}"),
        },
    }
}

const BASE_IMAGES_CHECK: &str = "Images de base des VM";

/// Chaque disque COW doit retrouver son image de base (sinon la VM ne démarre plus).
fn base_images_result(instances: &[Instance], instances_dir: &Path) -> CheckResult {
    let mut checked = 0;
    let mut broken = Vec::new();
    for instance in instances
        .iter()
        .filter(|i| i.status != InstanceStatus::Destroyed)
    {
        let disk_path = instances_dir.join(&instance.id).join("disk.qcow2");
        if !disk_path.is_file() {
            continue;
        }
        let label = instance.name.as_deref().unwrap_or(&instance.id);
        match disk::backing_file(&disk_path) {
            Ok(Some(base)) if !base.is_file() => {
                broken.push(format!("{label} → {} introuvable", base.display()));
            }
            Ok(_) => checked += 1,
            Err(err) => broken.push(format!("{label} : {err}")),
        }
    }

    if broken.is_empty() {
        return CheckResult {
            name: BASE_IMAGES_CHECK.to_string(),
            ok: true,
            detail: format!("{checked} disque(s) de VM vérifié(s)"),
        };
    }
    CheckResult {
        name: BASE_IMAGES_CHECK.to_string(),
        ok: false,
        detail: format!(
            "{}. Restaurez le fichier d'origine ; ne le re-téléchargez pas avec `virtum pull` \
             (une image différente rendrait le disque incohérent) — sinon recréez la VM.",
            broken.join(" ; ")
        ),
    }
}

const ORPHANS_CHECK: &str = "Restes orphelins";

/// Domaines `vm-…` et dossiers `instances/vm-…` sans entrée `metadata.json` (rien n'est supprimé).
fn check_orphans(paths: &config::VirtumPaths) -> CheckResult {
    let metadata = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(metadata::load_metadata(&paths.metadata)));
    let (metadata, domains) = match (metadata, libvirt::list_domain_names()) {
        (Ok(metadata), Ok(domains)) => (metadata, domains),
        (Err(err), _) | (_, Err(err)) => {
            return CheckResult {
                name: ORPHANS_CHECK.to_string(),
                ok: true,
                detail: format!("non vérifié ({err})"),
            };
        }
    };
    let known: Vec<&str> = metadata
        .instances
        .iter()
        .filter(|i| i.status != InstanceStatus::Destroyed)
        .map(|i| i.id.as_str())
        .collect();
    let dirs: Vec<String> = std::fs::read_dir(&paths.instances)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let mut orphans = find_orphans(&known, &domains, &dirs);
    // Domaines d'un autre Virtum (même libvirt, même nom de lab) : pas des orphelins à
    // nettoyer, et `virtum destroy` les refuserait.
    let (foreign, own): (Vec<String>, Vec<String>) = orphans.domains.into_iter().partition(|d| {
        matches!(
            crate::ownership::domain_owner(d),
            crate::ownership::Owner::Other(_)
        )
    });
    orphans.domains = own;
    let mut result = orphans_result(&orphans, &paths.instances);
    if !foreign.is_empty() {
        result.detail.push_str(&format!(
            " (ignorés, à un autre Virtum : {})",
            foreign.join(", ")
        ));
    }
    result
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Orphans {
    /// Domaines libvirt `vm-…` inconnus de `metadata.json`.
    domains: Vec<String>,
    /// Dossiers `instances/vm-…` sans metadata ni domaine.
    dirs: Vec<String>,
}

fn find_orphans(known: &[&str], domains: &[String], dirs: &[String]) -> Orphans {
    let is_orphan = |name: &String| name.starts_with("vm-") && !known.contains(&name.as_str());
    let mut orphans = Orphans {
        domains: domains.iter().filter(|d| is_orphan(d)).cloned().collect(),
        dirs: dirs
            .iter()
            .filter(|d| is_orphan(d) && !domains.contains(d))
            .cloned()
            .collect(),
    };
    orphans.domains.sort();
    orphans.dirs.sort();
    orphans
}

/// Avertissement non bloquant : liste les orphelins et la commande de nettoyage adaptée.
fn orphans_result(orphans: &Orphans, instances_dir: &Path) -> CheckResult {
    if orphans.domains.is_empty() && orphans.dirs.is_empty() {
        return CheckResult {
            name: ORPHANS_CHECK.to_string(),
            ok: true,
            detail: "aucun".to_string(),
        };
    }
    let mut parts = Vec::new();
    if !orphans.domains.is_empty() {
        parts.push(format!(
            "domaine(s) libvirt sans metadata : {}",
            orphans.domains.join(", ")
        ));
    }
    if !orphans.dirs.is_empty() {
        parts.push(format!(
            "dossier(s) instances/ sans VM : {}",
            orphans.dirs.join(", ")
        ));
    }

    // `virtum destroy` ne reconnaît que les ids générés (`vm-` + hex) ; le reste se nettoie à la main.
    let all: Vec<&String> = orphans.domains.iter().chain(&orphans.dirs).collect();
    let (destroyable, manual): (Vec<&String>, Vec<&String>) =
        all.into_iter().partition(|id| vm::is_virtum_vm_id(id));
    let mut hints = Vec::new();
    if !destroyable.is_empty() {
        let ids: Vec<&str> = destroyable.iter().map(|s| s.as_str()).collect();
        hints.push(format!("`virtum destroy {}`", ids.join(" ")));
    }
    for id in manual {
        if orphans.domains.contains(id) {
            hints.push(format!("`virsh undefine {id}`"));
        }
        let dir = instances_dir.join(id);
        if dir.is_dir() {
            hints.push(format!("`rm -r {}`", dir.display()));
        }
    }

    CheckResult {
        name: ORPHANS_CHECK.to_string(),
        ok: true,
        detail: format!(
            "{}. Vérifiez avant de nettoyer : {}",
            parts.join(" ; "),
            hints.join(", ")
        ),
    }
}

fn check_bridge_lan() -> CheckResult {
    const NAME: &str = "Réseau bridge (LAN physique)";
    let cap = bridge_network::scan_bridge_capability();

    if let Some(iface) = &cap.recommended_interface {
        let route_hint = bridge_network::default_route_interface()
            .map(|d| format!(" (route par défaut : {d})"))
            .unwrap_or_default();
        let mut detail = format!("Interface recommandée : {iface}{route_hint}");
        if !cap.ethernet_interfaces.is_empty() {
            detail.push_str(&format!(
                " (Ethernet : {})",
                cap.ethernet_interfaces.join(", ")
            ));
        }
        for w in &cap.warnings {
            detail.push_str(&format!(". {w}"));
        }
        return CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail,
        };
    }

    let mut detail = cap.warnings.join(". ");
    if detail.is_empty() {
        detail = "Aucune interface Ethernet détectée. Branchez un câble ou définissez \
                  `bridge_interface:` dans le Virtumfile. Le Wi‑Fi est souvent incompatible \
                  avec le bridge libvirt vers le LAN."
            .to_string();
    }
    detail.push_str(
        " Vérifiez aussi les groupes libvirt/kvm et un pare-feu local si les collègues \
          ne joignent pas la VM (étape 38+).",
    );

    CheckResult {
        name: NAME.to_string(),
        ok: false,
        detail,
    }
}

fn check_libvirt_default_network() -> CheckResult {
    const NAME: &str = "Réseau libvirt « default »";
    let output = match Command::new("virsh").args(["net-info", "default"]).output() {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return CheckResult {
                name: NAME.to_string(),
                ok: false,
                detail: "virsh introuvable (voir la vérification virsh ci-dessus).".to_string(),
            };
        }
        Err(e) => {
            return CheckResult {
                name: NAME.to_string(),
                ok: false,
                detail: format!("Impossible d'exécuter virsh net-info default: {e}"),
            };
        }
    };

    if !output.status.success() {
        return CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: format!(
                "Le réseau « default » est introuvable ou inaccessible: {}. \
                 Vérifiez que libvirt est installé et démarré \
                 (sudo systemctl enable --now libvirtd).",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        };
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let active = field_value(&text, "Active:").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    let autostart = field_value(&text, "Autostart:")
        .is_some_and(|v| v.eq_ignore_ascii_case("yes") || v.eq_ignore_ascii_case("enable"));

    let mut problems = Vec::new();
    if !active {
        problems.push("le réseau n'est pas actif (essayez: virsh net-start default)".to_string());
    }
    if !autostart {
        problems.push(
            "l'autostart n'est pas activé (essayez: virsh net-autostart default)".to_string(),
        );
    }

    if problems.is_empty() {
        CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: String::new(),
        }
    } else {
        CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: problems.join("; "),
        }
    }
}

fn field_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find(|line| line.starts_with(key))
        .map(|line| line[key.len()..].trim().to_string())
}

fn print_report(results: &[CheckResult]) -> i32 {
    let mut stdout = io::stdout().lock();
    let failed = results.iter().filter(|r| !r.ok).count();
    let passed = results.len() - failed;

    let _ = writeln!(stdout, "Virtum doctor\n");

    for result in results {
        if result.ok {
            if result.detail.is_empty() {
                let _ = writeln!(stdout, "  [ok]   {}", result.name);
            } else {
                let _ = writeln!(stdout, "  [ok]   {} — {}", result.name, result.detail);
            }
        } else {
            let _ = writeln!(stdout, "  [FAIL] {} — {}", result.name, result.detail);
        }
    }

    let _ = writeln!(stdout);
    if failed == 0 {
        let _ = writeln!(
            stdout,
            "Toutes les vérifications ont réussi ({passed}/{}).",
            results.len()
        );
        0
    } else {
        let _ = writeln!(
            stdout,
            "Résumé: {passed} réussie(s), {failed} échec(s) sur {} vérification(s).",
            results.len()
        );
        let _ = writeln!(
            stdout,
            "Corrigez les échecs ci-dessus avant d'utiliser virtum run."
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn find_orphans_ignores_known_and_foreign_domains() {
        let known = ["vm-39c32f26cb1b"];
        let domains = strings(&["vm-39c32f26cb1b", "vm-18a0ec445896", "vm-aa-test6", "win11"]);
        let dirs = strings(&["vm-39c32f26cb1b", "vm-aa-test6", "vm-perm-test"]);
        let orphans = find_orphans(&known, &domains, &dirs);
        assert_eq!(
            orphans.domains,
            strings(&["vm-18a0ec445896", "vm-aa-test6"])
        );
        // vm-aa-test6 a un domaine : compté une seule fois, côté domaines.
        assert_eq!(orphans.dirs, strings(&["vm-perm-test"]));
    }

    #[test]
    fn orphans_result_suggests_destroy_or_manual_cleanup() {
        let dir = std::env::temp_dir().join(format!("virtum-doctor-orph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vm-perm-test")).unwrap();
        let orphans = Orphans {
            domains: strings(&["vm-18a0ec445896", "vm-aa-test6"]),
            dirs: strings(&["vm-perm-test"]),
        };
        let result = orphans_result(&orphans, &dir);
        assert!(result.ok, "avertissement non bloquant");
        assert!(result.detail.contains("`virtum destroy vm-18a0ec445896`"));
        assert!(result.detail.contains("`virsh undefine vm-aa-test6`"));
        assert!(result.detail.contains("vm-perm-test`"));
        assert!(!result.detail.contains("virsh undefine vm-perm-test"));

        let none = orphans_result(&Orphans::default(), &dir);
        assert_eq!(none.detail, "aucun");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_images_check_detects_missing_base() {
        let dir = std::env::temp_dir().join(format!("virtum-doctor-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let instance_dir = dir.join("instances").join("vm-test01");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let base = dir.join("base.qcow2");
        crate::command::run_command(
            "qemu-img",
            &["create", "-f", "qcow2", base.to_str().unwrap(), "1G"],
        )
        .unwrap();
        disk::create_cow_disk(&base, &instance_dir.join("disk.qcow2"), None).unwrap();
        let instance: Instance = serde_json::from_str(
            r#"{"id":"vm-test01","name":"app","os":"ubuntu-24.04","status":"stopped",
                "user":"virtum","hostname":"app","created_at":"2026-10-29T09:00:00Z"}"#,
        )
        .unwrap();
        let instances = [instance];

        let ok = base_images_result(&instances, &dir.join("instances"));
        assert!(ok.ok, "{}", ok.detail);
        assert!(ok.detail.contains("1 disque"));

        std::fs::remove_file(&base).unwrap();
        let broken = base_images_result(&instances, &dir.join("instances"));
        assert!(!broken.ok);
        assert!(broken.detail.contains("app →"));
        assert!(broken.detail.contains("introuvable"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_virsh_net_info_fields() {
        let sample = r#"Name:           default
UUID:           abc
Active:         yes
Persistent:     yes
Autostart:      enable
"#;
        assert!(field_value(sample, "Active:")
            .unwrap()
            .eq_ignore_ascii_case("yes"));
        assert_eq!(field_value(sample, "Autostart:").unwrap(), "enable");
    }
}
