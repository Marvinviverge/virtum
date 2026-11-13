//! Vérification de l'identité d'un processus avant de lui envoyer un signal.
//!
//! Les PID enregistrés dans l'état Virtum (socat, virtiofsd) peuvent être périmés
//! (PID réutilisé) ou altérés : on ne signale qu'un processus de l'utilisateur courant
//! dont la ligne de commande correspond à ce que Virtum a lancé.

use std::path::Path;

use crate::fsutil::current_uid;

/// Arguments (`argv`) d'un processus, ou `None` s'il n'existe plus / n'est pas lisible.
fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

fn real_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

/// `true` si `pid` appartient à l'utilisateur courant, que `argv[0]` se termine par
/// `program` et qu'un argument contient `needle`.
pub fn process_matches(pid: u32, program: &str, needle: &str) -> bool {
    if pid == 0 || real_uid(pid) != Some(current_uid()) {
        return false;
    }
    let Some(args) = cmdline(pid) else {
        return false;
    };
    let exe_ok = args
        .first()
        .and_then(|a0| Path::new(a0).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == program);
    exe_ok && args.iter().skip(1).any(|a| a.contains(needle))
}

/// Envoie `sig` à `pid` seulement si [`process_matches`]. Retourne `true` si envoyé.
pub fn signal_if_matches(pid: u32, sig: i32, program: &str, needle: &str) -> bool {
    if !process_matches(pid, program, needle) {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, sig) == 0 }
}

/// PID des processus de l'utilisateur courant nommés `program` dont un argument
/// contient `needle`.
pub fn find_matching(program: &str, needle: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .filter(|pid| process_matches(*pid, program, needle))
        .collect()
}

/// Processus vivant (signal 0), sans tenir compte de son identité.
pub fn alive(pid: u32) -> bool {
    match i32::try_from(pid) {
        Ok(p) if p > 0 => unsafe { libc::kill(p, 0) == 0 },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_or_unrelated_process_is_not_matched() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        // Juste après spawn, /proc/<pid>/cmdline peut encore être celui du parent (exec en cours).
        for _ in 0..100 {
            if process_matches(pid, "sleep", "30") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(process_matches(pid, "sleep", "30"));
        // Bon processus, mauvais programme ou mauvais argument : refusé.
        assert!(!process_matches(pid, "socat", "30"));
        assert!(!process_matches(pid, "sleep", "TCP:"));
        assert!(!signal_if_matches(pid, libc::SIGTERM, "socat", "30"));
        assert!(alive(pid));
        assert!(signal_if_matches(pid, libc::SIGTERM, "sleep", "30"));
        let _ = child.wait();
        // PID 1 (root) n'appartient pas à l'utilisateur de test.
        if current_uid() != 0 {
            assert!(!process_matches(1, "systemd", ""));
        }
    }
}
