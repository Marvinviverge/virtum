use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::bridge_network;
use crate::config;
use crate::disk;
use crate::libvirt;
use crate::metadata::{self, Instance, InstanceStatus};
use crate::tr;
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
            let _ = writeln!(stdout, "Virtum doctor\n"); // i18n-ok
            let _ = writeln!(
                stdout,
                "{}",
                tr!(
                    fr: "  [FAIL] Arborescence Virtum — {err:#}",
                    en: "  [FAIL] Virtum directory tree — {err:#}"
                )
            );
            let _ = writeln!(
                stdout,
                "{}",
                tr!(
                    fr: "\nRésumé: impossible d'initialiser le répertoire Virtum.",
                    en: "\nSummary: cannot initialize the Virtum directory."
                )
            );
            return 1;
        }
    };

    let results = vec![
        check_binary(
            "qemu-img",
            &["--version"],
            &tr!(
                fr: "Installez qemu-utils (ex. sudo apt install qemu-utils).",
                en: "Install qemu-utils (e.g. sudo apt install qemu-utils)."
            ),
        ),
        check_binary(
            "virsh",
            &["--version"],
            &tr!(
                fr: "Installez libvirt-clients (ex. sudo apt install libvirt-clients).",
                en: "Install libvirt-clients (e.g. sudo apt install libvirt-clients)."
            ),
        ),
        check_binary(
            "cloud-localds",
            &["--help"],
            &tr!(
                fr: "Installez cloud-image-utils (ex. sudo apt install cloud-image-utils).",
                en: "Install cloud-image-utils (e.g. sudo apt install cloud-image-utils)."
            ),
        ),
        check_binary(
            "ssh",
            &["-V"],
            &tr!(
                fr: "Installez openssh-client (ex. sudo apt install openssh-client).",
                en: "Install openssh-client (e.g. sudo apt install openssh-client)."
            ),
        ),
        check_binary(
            "socat",
            &["-V"],
            &tr!(
                fr: "Requis pour `ports:` dans le Virtumfile (ex. sudo apt install socat).",
                en: "Required for `ports:` in the Virtumfile (e.g. sudo apt install socat)."
            ),
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
            detail: tr!(
                fr: "{name} est présent mais a échoué ({install_hint})",
                en: "{name} is present but failed ({install_hint})"
            ),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: tr!(
                fr: "{name} introuvable dans le PATH. {install_hint}",
                en: "{name} not found in PATH. {install_hint}"
            ),
        },
        Err(e) => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: tr!(fr: "Impossible d'exécuter {name}: {e}", en: "Cannot run {name}: {e}"),
        },
    }
}

/// Ansible est optionnel (section `ansible:`, v0.8) : jamais bloquant.
fn check_ansible() -> CheckResult {
    let detail = match Command::new("ansible-playbook").arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or(&tr!(fr: "présent", en: "present"))
            .trim()
            .to_string(),
        _ => tr!(
            fr: "absent (optionnel : requis seulement pour la section `ansible:` du Virtumfile ; \
                 ex. sudo apt install ansible-core)",
            en: "missing (optional: only required for the `ansible:` section of the Virtumfile; \
                 e.g. sudo apt install ansible-core)"
        ),
    };
    CheckResult {
        name: "ansible-playbook".to_string(),
        ok: true,
        detail,
    }
}

/// virtiofsd sert seulement aux dossiers partagés (`mounts:`) : jamais bloquant. Sans lui,
/// VM et labs fonctionnent ; seuls les partages sont indisponibles (Debian 12, Ubuntu 22.04 :
/// pas de paquet `virtiofsd`, et l'ancien virtiofsd en C de QEMU n'est pas compatible).
fn check_virtiofsd() -> CheckResult {
    let name = tr!(
        fr: "virtiofsd (dossiers partagés, optionnel)",
        en: "virtiofsd (shared folders, optional)"
    );
    let unavailable = |reason: String| CheckResult {
        name: name.clone(),
        ok: true,
        detail: tr!(
            fr: "{reason} : les VM et les labs fonctionnent, mais pas les dossiers partagés \
                 (`mounts:`).",
            en: "{reason}: VMs and labs work, but shared folders (`mounts:`) do not."
        ),
    };
    let Some(path) = crate::mounts::resolve_virtiofsd_path() else {
        return unavailable(tr!(
            fr: "absent (sudo apt install virtiofsd ; paquet inexistant sur Debian 12 et \
                 Ubuntu 22.04)",
            en: "missing (sudo apt install virtiofsd; no such package on Debian 12 and \
                 Ubuntu 22.04)"
        ));
    };
    match Command::new(&path).arg("--version").output() {
        Ok(output) if output.status.success() => {
            // Pas de note uidmap : Virtum lance toujours virtiofsd avec --sandbox=none
            // (SECURITY.md, Limites) ; installer uidmap n'y change rien.
            // Les partages sont en lecture seule par défaut : sans moyen de l'imposer côté
            // hôte, seuls les partages `rw` fonctionnent.
            if crate::mounts::read_only_backend(&path).is_none() {
                return CheckResult {
                    name: format!("{name} ({})", path.display()),
                    ok: true,
                    detail: tr!(
                        fr: "partages en lecture seule impossibles : ce virtiofsd n'a pas \
                             --readonly et bubblewrap est absent (sudo apt install bubblewrap). \
                             Seuls les partages `rw` fonctionnent.",
                        en: "read-only shares unavailable: this virtiofsd has no \
                             --readonly and bubblewrap is missing (sudo apt install bubblewrap). \
                             Only `rw` shares work."
                    ),
                };
            }
            CheckResult {
                name: format!("{name} ({})", path.display()),
                ok: true,
                detail: String::new(),
            }
        }
        Ok(_) => {
            let path = path.display();
            unavailable(tr!(
                fr: "{path} ne répond pas à --version (ancien virtiofsd en C fourni par QEMU, \
                     incompatible ; il faut le virtiofsd en Rust, paquet virtiofsd)",
                en: "{path} does not answer --version (old C virtiofsd shipped with QEMU, \
                     incompatible; the Rust virtiofsd is required, package virtiofsd)"
            ))
        }
        Err(e) => {
            let path = path.display();
            unavailable(tr!(
                fr: "impossible d'exécuter {path} ({e})",
                en: "cannot run {path} ({e})"
            ))
        }
    }
}

fn check_apparmor_virtiofs() -> CheckResult {
    const NAME: &str = "AppArmor virtiofs (Virtum)"; // i18n-ok
    if !config::uses_var_lib_storage() {
        return CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: tr!(
                fr: "Stockage sous ~/.virtum — règle dédiée non requise.",
                en: "Storage under ~/.virtum — no dedicated rule required."
            ),
        };
    }
    let snippet =
        std::path::Path::new("/etc/apparmor.d/abstractions/libvirt-qemu.d/virtum-virtiofs");
    if !snippet.is_file() {
        return CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: {
                let dir = env!("CARGO_MANIFEST_DIR");
                tr!(
                    fr: "Les sockets virtiofsd sous /var/lib/virtum sont bloqués par AppArmor \
                         tant que la règle n'est pas installée :\n\
                           sudo {dir}/scripts/setup-apparmor-virtiofs.sh",
                    en: "virtiofsd sockets under /var/lib/virtum are blocked by AppArmor \
                         until the rule is installed:\n\
                           sudo {dir}/scripts/setup-apparmor-virtiofs.sh"
                )
            },
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
            detail: {
                let snippet = snippet.display();
                let dir = env!("CARGO_MANIFEST_DIR");
                tr!(
                    fr: "{snippet} existe mais ne contient pas la règle Virtum attendue. \
                         Réinstallez : sudo {dir}/scripts/setup-apparmor-virtiofs.sh",
                    en: "{snippet} exists but does not contain the expected Virtum rule. \
                         Reinstall: sudo {dir}/scripts/setup-apparmor-virtiofs.sh"
                )
            },
        }
    }
}

fn check_kvm() -> CheckResult {
    const NAME: &str = "KVM (/dev/kvm)"; // i18n-ok
    match OpenOptions::new().read(true).write(true).open(KVM_DEVICE) {
        Ok(_) => CheckResult {
            name: NAME.to_string(),
            ok: true,
            detail: String::new(),
        },
        Err(e) => CheckResult {
            name: NAME.to_string(),
            ok: false,
            detail: tr!(
                fr: "Accès lecture/écriture à {KVM_DEVICE} impossible ({e}). \
                     Vérifiez que la virtualisation KVM est activée dans le BIOS/UEFI \
                     et que votre utilisateur est dans le groupe kvm \
                     (sudo usermod -aG kvm $USER, puis reconnectez la session).",
                en: "Cannot open {KVM_DEVICE} for reading and writing ({e}). \
                     Check that KVM virtualization is enabled in the BIOS/UEFI \
                     and that your user is in the kvm group \
                     (sudo usermod -aG kvm $USER, then log in again)."
            ),
        },
    }
}

fn check_groups() -> CheckResult {
    let name = tr!(fr: "Groupes libvirt et kvm", en: "libvirt and kvm groups");
    let groups = match user_groups() {
        Ok(g) => g,
        Err(e) => {
            return CheckResult {
                name,
                ok: false,
                detail: tr!(
                    fr: "Impossible de lire les groupes de l'utilisateur: {e}",
                    en: "Cannot read the user's groups: {e}"
                ),
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
            name,
            ok: true,
            detail: String::new(),
        }
    } else {
        let listed = missing.join(", ");
        let joined = missing.join(",");
        CheckResult {
            name,
            ok: false,
            detail: tr!(
                fr: "Groupes manquants: {listed}. Ajoutez-vous avec \
                     sudo usermod -aG {joined} $USER puis reconnectez votre session.",
                en: "Missing groups: {listed}. Add yourself with \
                     sudo usermod -aG {joined} $USER then log in again."
            ),
        }
    }
}

fn user_groups() -> io::Result<Vec<String>> {
    let output = Command::new("id").arg("-Gn").output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(tr!(
            fr: "id -Gn a échoué: {stderr}",
            en: "id -Gn failed: {stderr}"
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect())
}

fn check_virtum_storage(paths: &config::VirtumPaths) -> CheckResult {
    let name = tr!(
        fr: "Stockage Virtum (libvirt système)",
        en: "Virtum storage (system libvirt)"
    );
    let root = paths.root.display();
    if config::uses_var_lib_storage() {
        return CheckResult {
            name,
            ok: true,
            detail: tr!(fr: "Répertoire actif : {root}", en: "Active directory: {root}"),
        };
    }

    let var_lib = config::VAR_LIB_VIRTUM;
    CheckResult {
        name,
        ok: false,
        detail: tr!(
            fr: "Stockage sous {root} — QEMU système ne peut pas lire $HOME/.virtum (AppArmor). \
                 Exécutez une fois : sudo ./scripts/setup-var-lib.sh \
                 (crée {var_lib}/ et copie l'image si besoin).",
            en: "Storage under {root} — system QEMU cannot read $HOME/.virtum (AppArmor). \
                 Run once: sudo ./scripts/setup-var-lib.sh \
                 (creates {var_lib}/ and copies the image if needed)."
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
            name: base_images_check(),
            ok: false,
            detail: format!("{err:#}"),
        },
    }
}

fn base_images_check() -> String {
    tr!(fr: "Images de base des VM", en: "VM base images")
}

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
                let base = base.display();
                broken.push(
                    tr!(fr: "{label} → {base} introuvable", en: "{label} → {base} not found"),
                );
            }
            Ok(_) => checked += 1,
            Err(err) => broken.push(format!("{label} : {err}")),
        }
    }

    if broken.is_empty() {
        return CheckResult {
            name: base_images_check(),
            ok: true,
            detail: tr!(
                fr: "{checked} disque(s) de VM vérifié(s)",
                en: "{checked} VM disk(s) checked"
            ),
        };
    }
    let broken = broken.join(&crate::tr!(fr: " ; ", en: "; "));
    CheckResult {
        name: base_images_check(),
        ok: false,
        detail: tr!(
            fr: "{broken}. Restaurez le fichier d'origine ; ne le re-téléchargez pas avec \
                 `virtum pull` (une image différente rendrait le disque incohérent) — sinon \
                 recréez la VM.",
            en: "{broken}. Restore the original file; do not download it again with \
                 `virtum pull` (a different image would make the disk inconsistent) — otherwise \
                 recreate the VM."
        ),
    }
}

fn orphans_check() -> String {
    tr!(fr: "Restes orphelins", en: "Orphaned leftovers")
}

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
                name: orphans_check(),
                ok: true,
                detail: tr!(fr: "non vérifié ({err})", en: "not checked ({err})"),
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
        let foreign = foreign.join(", ");
        result.detail.push_str(&tr!(
            fr: " (ignorés, à un autre Virtum : {foreign})",
            en: " (ignored, owned by another Virtum: {foreign})"
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
            name: orphans_check(),
            ok: true,
            detail: tr!(fr: "aucun", en: "none"),
        };
    }
    let mut parts = Vec::new();
    if !orphans.domains.is_empty() {
        let domains = orphans.domains.join(", ");
        parts.push(tr!(
            fr: "domaine(s) libvirt sans metadata : {domains}",
            en: "libvirt domain(s) without metadata: {domains}"
        ));
    }
    if !orphans.dirs.is_empty() {
        let dirs = orphans.dirs.join(", ");
        parts.push(tr!(
            fr: "dossier(s) instances/ sans VM : {dirs}",
            en: "instances/ folder(s) without a VM: {dirs}"
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

    let parts = parts.join(&crate::tr!(fr: " ; ", en: "; "));
    let hints = hints.join(", ");
    CheckResult {
        name: orphans_check(),
        ok: true,
        detail: tr!(
            fr: "{parts}. Vérifiez avant de nettoyer : {hints}",
            en: "{parts}. Check before cleaning up: {hints}"
        ),
    }
}

fn check_bridge_lan() -> CheckResult {
    let name = tr!(fr: "Réseau bridge (LAN physique)", en: "Bridge network (physical LAN)");
    let cap = bridge_network::scan_bridge_capability();

    if let Some(iface) = &cap.recommended_interface {
        let route_hint = bridge_network::default_route_interface()
            .map(|d| tr!(fr: " (route par défaut : {d})", en: " (default route: {d})"))
            .unwrap_or_default();
        let mut detail = tr!(
            fr: "Interface recommandée : {iface}{route_hint}",
            en: "Recommended interface: {iface}{route_hint}"
        );
        if !cap.ethernet_interfaces.is_empty() {
            let ethernet = cap.ethernet_interfaces.join(", ");
            detail.push_str(&tr!(fr: " (Ethernet : {ethernet})", en: " (Ethernet: {ethernet})"));
        }
        for w in &cap.warnings {
            detail.push_str(&format!(". {w}"));
        }
        return CheckResult {
            name,
            ok: true,
            detail,
        };
    }

    let mut detail = cap.warnings.join(". ");
    if detail.is_empty() {
        detail = tr!(
            fr: "Aucune interface Ethernet détectée. Branchez un câble ou définissez \
                 `bridge_interface:` dans le Virtumfile. Le Wi‑Fi est souvent incompatible \
                 avec le bridge libvirt vers le LAN.",
            en: "No Ethernet interface detected. Plug in a cable or set \
                 `bridge_interface:` in the Virtumfile. Wi‑Fi is often incompatible \
                 with a libvirt bridge to the LAN."
        );
    }
    detail.push_str(&tr!(
        fr: " Vérifiez aussi les groupes libvirt/kvm et un pare-feu local si les collègues \
             ne joignent pas la VM (étape 38+).",
        en: " Also check the libvirt/kvm groups and any local firewall if colleagues \
             cannot reach the VM (step 38+)."
    ));

    CheckResult {
        name,
        ok: false,
        detail,
    }
}

fn check_libvirt_default_network() -> CheckResult {
    let name = tr!(fr: "Réseau libvirt « default »", en: "libvirt network 'default'");
    let output = match crate::libvirt::virsh_command()
        .args(["net-info", "default"])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return CheckResult {
                name,
                ok: false,
                detail: tr!(
                    fr: "virsh introuvable (voir la vérification virsh ci-dessus).",
                    en: "virsh not found (see the virsh check above)."
                ),
            };
        }
        Err(e) => {
            return CheckResult {
                name,
                ok: false,
                detail: tr!(
                    fr: "Impossible d'exécuter virsh net-info default: {e}",
                    en: "Cannot run virsh net-info default: {e}"
                ),
            };
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return CheckResult {
            name,
            ok: false,
            detail: tr!(
                fr: "Le réseau « default » est introuvable ou inaccessible: {stderr}. \
                     Vérifiez que libvirt est installé et démarré \
                     (sudo systemctl enable --now libvirtd), puis : \
                     virsh -c qemu:///system net-list --all",
                en: "The 'default' network is missing or unreachable: {stderr}. \
                     Check that libvirt is installed and running \
                     (sudo systemctl enable --now libvirtd), then: \
                     virsh -c qemu:///system net-list --all"
            ),
        };
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let active = field_value(&text, "Active:").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    let autostart = field_value(&text, "Autostart:")
        .is_some_and(|v| v.eq_ignore_ascii_case("yes") || v.eq_ignore_ascii_case("enable"));

    let mut problems = Vec::new();
    if !active {
        problems.push(tr!(
            fr: "le réseau n'est pas actif (virsh -c qemu:///system net-start default)",
            en: "the network is not active (virsh -c qemu:///system net-start default)"
        ));
    }
    if !autostart {
        problems.push(tr!(
            fr: "l'autostart n'est pas activé (virsh -c qemu:///system net-autostart default)",
            en: "autostart is not enabled (virsh -c qemu:///system net-autostart default)"
        ));
    }

    if problems.is_empty() {
        CheckResult {
            name,
            ok: true,
            detail: String::new(),
        }
    } else {
        CheckResult {
            name,
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

    let _ = writeln!(stdout, "Virtum doctor\n"); // i18n-ok

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
        let total = results.len();
        let _ = writeln!(
            stdout,
            "{}",
            tr!(
                fr: "Toutes les vérifications ont réussi ({passed}/{total}).",
                en: "All checks passed ({passed}/{total})."
            )
        );
        0
    } else {
        let total = results.len();
        let _ = writeln!(
            stdout,
            "{}",
            tr!(
                fr: "Résumé: {passed} réussie(s), {failed} échec(s) sur {total} vérification(s).",
                en: "Summary: {passed} passed, {failed} failed out of {total} check(s)."
            )
        );
        let _ = writeln!(
            stdout,
            "{}",
            tr!(
                fr: "Corrigez les échecs ci-dessus avant d'utiliser virtum run.",
                en: "Fix the failures above before using virtum run."
            )
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
