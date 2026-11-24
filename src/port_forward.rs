//! Port mapping hôte → invité (`ports:`, `bind: lan`) via `socat`.
//!
//! Les proxies tournent en processus séparé (survit à la fin de `virtum up`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bridge_network;
use crate::metadata::{Instance, InstanceStatus, Metadata};
use crate::tr;

const FORWARD_PID_FILE: &str = "port-forwards.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PortBind {
    /// `127.0.0.1` uniquement (défaut v0.2).
    #[default]
    Local,
    /// Toutes les interfaces — joignable depuis le LAN (Wi‑Fi, partage téléphone, Ethernet).
    Lan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ForwardPid {
    host_port: u16,
    guest_port: u16,
    bind: PortBind,
    pid: u32,
}

fn pid_file(instance_dir: &Path) -> PathBuf {
    instance_dir.join(FORWARD_PID_FILE)
}

fn listen_bind_address(bind: PortBind) -> &'static str {
    match bind {
        PortBind::Local => "127.0.0.1",
        PortBind::Lan => "0.0.0.0",
    }
}

/// Vérifie que le port hôte n'est pas déjà pris sur l'adresse d'écoute.
pub fn ensure_host_port_available(host_port: u16, bind: PortBind) -> Result<()> {
    use std::net::TcpListener;
    let addr = format!("{}:{host_port}", listen_bind_address(bind));
    TcpListener::bind(&addr).with_context(|| {
        let scope = match bind {
            PortBind::Local => "127.0.0.1",
            PortBind::Lan => "0.0.0.0 (LAN)", // i18n-ok : identique en anglais
        };
        tr!(
            fr: "Le port hôte {host_port} est déjà utilisé sur {scope}. \
             Choisissez un autre port dans le Virtumfile (ex. 18080:80) \
             ou arrêtez le processus qui l'occupe.",
            en: "Host port {host_port} is already in use on {scope}. \
             Choose another port in the Virtumfile (e.g. 18080:80) \
             or stop the process using it."
        )
    })?;
    Ok(())
}

pub fn ensure_ports_available(forwards: &[PortForward], bind: PortBind) -> Result<()> {
    for pf in forwards {
        ensure_host_port_available(pf.host_port, bind)?;
    }
    Ok(())
}

/// Pas de doublon de port hôte dans un Virtumfile.
pub fn validate_lab_port_uniqueness(machines: &[(String, Vec<PortForward>)]) -> Result<()> {
    let mut seen = HashSet::new();
    for (machine, forwards) in machines {
        for pf in forwards {
            if !seen.insert(pf.host_port) {
                bail!(tr!(
                    fr: "Port hôte {} en double dans le Virtumfile (machine '{machine}'). \
                     Chaque mapping doit utiliser un port hôte unique.",
                    en: "Duplicate host port {} in the Virtumfile (machine '{machine}'). \
                     Each mapping must use a unique host port.",
                    pf.host_port
                ));
            }
        }
    }
    Ok(())
}

/// Conflit avec une autre instance Virtum (metadata).
pub fn ensure_ports_not_in_metadata(
    metadata: &Metadata,
    skip_vm_id: Option<&str>,
    forwards: &[PortForward],
) -> Result<()> {
    for inst in &metadata.instances {
        if inst.status == InstanceStatus::Destroyed {
            continue;
        }
        if skip_vm_id == Some(inst.id.as_str()) {
            continue;
        }
        let Some(existing) = &inst.port_forwards else {
            continue;
        };
        for pf in forwards {
            for other in existing {
                if other.host_port == pf.host_port {
                    let label = inst.name.as_deref().unwrap_or(&inst.id);
                    bail!(tr!(
                        fr: "Le port hôte {} est déjà mappé par l'instance '{label}' ({id}).",
                        en: "Host port {} is already mapped by instance '{label}' ({id}).",
                        pf.host_port,
                        id = inst.id
                    ));
                }
            }
        }
    }
    Ok(())
}

fn ensure_socat() -> Result<()> {
    let ok = Command::new("which")
        .arg("socat")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return Ok(());
    }
    bail!(tr!(
        fr: "socat est requis pour le mapping de ports (`ports:` dans le Virtumfile). \
         Installez-le : sudo apt install socat",
        en: "socat is required for port mapping (`ports:` in the Virtumfile). \
         Install it: sudo apt install socat"
    ));
}

/// Démarre les redirections TCP (processus `socat` détachés).
pub fn start_instance_proxies(
    instance_dir: &Path,
    guest_ip: &str,
    forwards: &[PortForward],
    bind: PortBind,
) -> Result<()> {
    stop_instance_proxies(instance_dir);
    if forwards.is_empty() {
        return Ok(());
    }
    ensure_socat()?;
    ensure_ports_available(forwards, bind)?;

    if bind == PortBind::Lan {
        if let Some(ip) = bridge_network::host_primary_ipv4() {
            crate::verr!(
                fr: "Ports exposés sur le LAN (écoute 0.0.0.0) — IP hôte typique : {ip}",
                en: "Ports exposed on the LAN (listening on 0.0.0.0) — typical host IP: {ip}"
            );
        } else {
            crate::verr!(
                fr: "Ports exposés sur le LAN (0.0.0.0) ; impossible de détecter l'IP hôte (ip route).",
                en: "Ports exposed on the LAN (0.0.0.0); cannot detect the host IP (ip route)."
            );
        }
        crate::verr!(
            fr: "Si un pare-feu bloque l'accès (ufw), autorisez le port ou testez depuis le même réseau.",
            en: "If a firewall blocks access (ufw), allow the port or test from the same network."
        );
    }

    let mut records = Vec::new();
    for &pf in forwards {
        let pid = spawn_socat_forward(guest_ip, pf, bind)?;
        records.push(ForwardPid {
            host_port: pf.host_port,
            guest_port: pf.guest_port,
            bind,
            pid,
        });
    }

    let path = pid_file(instance_dir);
    let content = serde_json::to_string_pretty(&records)
        .context(tr!(fr: "sérialisation port-forwards", en: "serializing port forwards"))?;
    crate::fsutil::write_atomic(&path, format!("{content}\n").as_bytes(), 0o644)?;
    Ok(())
}

/// Version de socat (`socat -V`), ex. `(1, 8)`.
fn socat_version() -> Option<(u32, u32)> {
    let output = Command::new("socat").arg("-V").output().ok()?;
    parse_socat_version(&String::from_utf8_lossy(&output.stdout))
}

fn parse_socat_version(text: &str) -> Option<(u32, u32)> {
    let rest = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("socat version "))?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// `ACCEPT-FD` (socket d'écoute transmis par Virtum) existe depuis socat 1.8.
fn socat_accepts_fd(version: Option<(u32, u32)>) -> bool {
    version.is_some_and(|v| v >= (1, 8))
}

/// Lance socat pour une redirection de port.
///
/// Avec socat ≥ 1.8, Virtum lie lui-même le port (`TcpListener`) puis transmet le socket à
/// socat (`ACCEPT-FD:3`) : le port n'est jamais libre entre la vérification et l'écoute.
/// Avec un socat plus ancien (Debian 12, Ubuntu 22.04), socat lie le port lui-même
/// (`TCP-LISTEN`) : un autre utilisateur local qui prendrait le port dans l'intervalle
/// empêche socat de démarrer, ce qui fait échouer la commande (refus de service possible,
/// pas de détournement). Dans les deux cas, socat qui s'arrête tout de suite fait échouer
/// la commande au lieu d'annoncer une redirection morte.
fn spawn_socat_forward(guest_ip: &str, pf: PortForward, bind: PortBind) -> Result<u32> {
    use std::net::TcpListener;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let listen_addr = listen_bind_address(bind);
    let listener = TcpListener::bind((listen_addr, pf.host_port)).with_context(|| {
        tr!(
            fr: "Le port hôte {} est déjà utilisé sur {listen_addr}. \
             Choisissez un autre port dans le Virtumfile ou arrêtez le processus qui l'occupe.",
            en: "Host port {} is already in use on {listen_addr}. \
             Choose another port in the Virtumfile or stop the process using it.",
            pf.host_port
        )
    })?;
    let target = format!("TCP:{guest_ip}:{}", pf.guest_port);
    let accept_fd = socat_accepts_fd(socat_version());
    let mut cmd = Command::new("socat");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !accept_fd {
        // socat < 1.8 : il lie le port lui-même ; Virtum libère le sien juste avant.
        drop(listener);
        cmd.arg(format!(
            "TCP-LISTEN:{},bind={listen_addr},reuseaddr,fork",
            pf.host_port
        ))
        .arg(&target);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| {
            tr!(
                fr: "Impossible de lancer socat pour {listen_addr}:{} → {guest_ip}:{}",
                en: "Cannot start socat for {listen_addr}:{} → {guest_ip}:{}",
                pf.host_port,
                pf.guest_port
            )
        })?;
        return confirm_socat_started(child, listen_addr, guest_ip, pf);
    }
    let fd = listener.as_raw_fd();
    cmd.arg(format!("ACCEPT-FD:{SOCAT_LISTEN_FD},fork"))
        .arg(&target);
    unsafe {
        cmd.pre_exec(move || {
            // Le socket devient le descripteur 3 de socat, sans FD_CLOEXEC.
            if fd == SOCAT_LISTEN_FD {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, SOCAT_LISTEN_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| {
        tr!(
            fr: "Impossible de lancer socat pour {listen_addr}:{} → {guest_ip}:{}",
            en: "Cannot start socat for {listen_addr}:{} → {guest_ip}:{}",
            pf.host_port,
            pf.guest_port
        )
    })?;
    // socat détient maintenant le socket ; Virtum peut fermer sa copie.
    drop(listener);
    confirm_socat_started(child, listen_addr, guest_ip, pf)
}

/// socat encore en vie après un court délai : la redirection écoute.
fn confirm_socat_started(
    mut child: std::process::Child,
    listen_addr: &str,
    guest_ip: &str,
    pf: PortForward,
) -> Result<u32> {
    let deadline = std::time::Instant::now() + SOCAT_STARTUP_CHECK;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .context(tr!(fr: "Impossible de suivre socat", en: "Cannot monitor socat"))?
        {
            bail!(tr!(
                fr: "socat s'est arrêté au démarrage ({status}) pour {listen_addr}:{} → {guest_ip}:{}. \
                 Le port a peut-être été pris entre-temps ; vérifiez-le (ss -ltnp) et relancez.",
                en: "socat stopped at startup ({status}) for {listen_addr}:{} → {guest_ip}:{}. \
                 The port may have been taken in the meantime; check it (ss -ltnp) and retry.",
                pf.host_port,
                pf.guest_port
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(child.id())
}

/// Descripteur sur lequel socat reçoit le socket d'écoute.
const SOCAT_LISTEN_FD: i32 = 3;
/// Délai pendant lequel un arrêt immédiat de socat est détecté.
const SOCAT_STARTUP_CHECK: std::time::Duration = std::time::Duration::from_millis(250);

/// Arrête les processus socat enregistrés pour cette instance.
///
/// Un PID n'est signalé que s'il s'agit bien d'un socat de l'utilisateur courant vers le
/// port invité enregistré (PID réutilisé ou fichier altéré : ignoré).
pub fn stop_instance_proxies(instance_dir: &Path) {
    let path = pid_file(instance_dir);
    if let Ok(Some(content)) = crate::fsutil::read_owned(&path) {
        if let Ok(records) = serde_json::from_str::<Vec<ForwardPid>>(&content) {
            for record in records {
                let needle = format!(":{}", record.guest_port);
                if crate::procs::signal_if_matches(record.pid, libc::SIGTERM, "socat", &needle) {
                    wait_for_exit(record.pid);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Attend brièvement la fin d'un socat signalé (le port est libéré pour la relance).
fn wait_for_exit(pid: u32) {
    for _ in 0..50 {
        if !crate::procs::alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub fn format_forward_list(forwards: &[PortForward], bind: PortBind) -> String {
    if forwards.is_empty() {
        return "-".to_string();
    }
    let host_ip = bridge_network::host_primary_ipv4();
    forwards
        .iter()
        .map(|pf| match bind {
            PortBind::Local => format!("127.0.0.1:{}→{}", pf.host_port, pf.guest_port),
            PortBind::Lan => {
                if let Some(ref ip) = host_ip {
                    format!("{ip}:{}→{}", pf.host_port, pf.guest_port)
                } else {
                    format!("LAN:{}→{}", pf.host_port, pf.guest_port)
                }
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn print_forward_lines(forwards: &[PortForward], bind: PortBind) {
    let lan_ip = bridge_network::host_primary_ipv4();
    for pf in forwards {
        crate::vout!(
            fr: "  Forward (local): http://127.0.0.1:{} → invité :{}",
            en: "  Forward (local): http://127.0.0.1:{} → guest :{}",
            pf.host_port,
            pf.guest_port
        );
        if bind == PortBind::Lan {
            if let Some(ref ip) = lan_ip {
                crate::vout!(
                    fr: "  Forward (LAN):   http://{ip}:{} → invité :{}  (téléphone / collègue même réseau)",
                    en: "  Forward (LAN):   http://{ip}:{} → guest :{}  (phone / colleague on the same network)",
                    pf.host_port,
                    pf.guest_port
                );
            } else {
                crate::vout!(
                    fr: "  Forward (LAN):   http://<IP-hôte>:{} → invité :{}  (écoute 0.0.0.0, voir `ip route`)",
                    en: "  Forward (LAN):   http://<host-IP>:{} → guest :{}  (listening on 0.0.0.0, see `ip route`)",
                    pf.host_port,
                    pf.guest_port
                );
            }
        }
    }
}

/// Virtumfile modifié alors que la VM existe déjà (sans recréer).
pub fn ensure_forwards_match_existing(
    machine_name: &str,
    spec_ports: &[PortForward],
    spec_bind: PortBind,
    existing: &Instance,
) -> Result<()> {
    let stored = existing.port_forwards.as_deref().unwrap_or(&[]);
    let stored_bind = existing.port_bind.unwrap_or(PortBind::Local);
    if stored == spec_ports && stored_bind == spec_bind {
        return Ok(());
    }
    if stored.is_empty() && !spec_ports.is_empty() {
        bail!(tr!(
            fr: "La machine '{machine_name}' existe sans mapping de ports. \
             Utilisez `virtum apply -f <Virtumfile>` ou `virtum down --destroy` puis `virtum up`.",
            en: "The machine '{machine_name}' exists without port mapping. \
             Run `virtum apply -f <Virtumfile>` or `virtum down --destroy` then `virtum up`."
        ));
    }
    if !stored.is_empty() && spec_ports.is_empty() {
        bail!(tr!(
            fr: "La machine '{machine_name}' a des ports mappés mais le Virtumfile n'en définit plus. \
             Utilisez `virtum apply -f <Virtumfile>` ou `virtum down --destroy` puis `virtum up`.",
            en: "The machine '{machine_name}' has mapped ports but the Virtumfile no longer defines any. \
             Run `virtum apply -f <Virtumfile>` or `virtum down --destroy` then `virtum up`."
        ));
    }
    bail!(tr!(
        fr: "Les ports ou `bind` du Virtumfile ne correspondent pas à l'instance '{machine_name}' ({id}). \
         Utilisez `virtum apply -f <Virtumfile>`.",
        en: "The Virtumfile `ports` or `bind` do not match the instance '{machine_name}' ({id}). \
         Run `virtum apply -f <Virtumfile>`.",
        id = existing.id
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socat_installed() -> bool {
        Command::new("socat").arg("-V").output().is_ok()
    }

    /// Audit : le port n'est jamais libre entre la vérification et l'écoute de socat, et
    /// un port déjà pris fait échouer le lancement au lieu d'annoncer une redirection morte.
    #[test]
    fn socat_inherits_bound_port_and_refuses_taken_port() {
        if !socat_installed() {
            return;
        }
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let pf = PortForward {
            host_port: port,
            guest_port: 9,
        };
        let pid = spawn_socat_forward("127.0.0.1", pf, PortBind::Local).unwrap();
        // Le port est tenu par socat : un autre bind échoue.
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        assert!(crate::procs::process_matches(pid, "socat", ":9"));
        assert!(crate::procs::signal_if_matches(
            pid,
            libc::SIGTERM,
            "socat",
            ":9"
        ));
        wait_for_exit(pid);

        let squatter = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert!(spawn_socat_forward("127.0.0.1", pf, PortBind::Local).is_err());
        drop(squatter);
    }

    #[test]
    fn socat_version_selects_accept_fd_or_tcp_listen() {
        let debian12 = "socat by Gerhard Rieger and contributors - see www.dest-unreach.org\n\
                        socat version 1.7.4.4 on 06 Nov 2022 08:15:51\n";
        let ubuntu2404 = "socat version 1.8.0.0 on 03 Jul 2026 12:45:49\n";
        assert_eq!(parse_socat_version(debian12), Some((1, 7)));
        assert_eq!(parse_socat_version(ubuntu2404), Some((1, 8)));
        assert_eq!(parse_socat_version("autre chose"), None);
        assert!(!socat_accepts_fd(Some((1, 7))));
        assert!(socat_accepts_fd(Some((1, 8))));
        assert!(socat_accepts_fd(Some((2, 0))));
        // Version illisible : repli TCP-LISTEN, qui fonctionne avec tout socat.
        assert!(!socat_accepts_fd(None));
    }

    #[test]
    fn listen_bind_address_local_and_lan() {
        assert_eq!(listen_bind_address(PortBind::Local), "127.0.0.1");
        assert_eq!(listen_bind_address(PortBind::Lan), "0.0.0.0");
    }

    #[test]
    fn validate_lab_port_uniqueness_rejects_duplicate() {
        let machines = vec![
            (
                "web".into(),
                vec![PortForward {
                    host_port: 18080,
                    guest_port: 80,
                }],
            ),
            (
                "api".into(),
                vec![PortForward {
                    host_port: 18080,
                    guest_port: 8080,
                }],
            ),
        ];
        assert!(validate_lab_port_uniqueness(&machines).is_err());
    }
}
