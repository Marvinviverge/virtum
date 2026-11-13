//! Exécution parallèle des machines d'un `virtum up` (v0.9).
//!
//! Un fil d'exécution par emplacement (au plus `parallel`), chacun avec son propre runtime
//! tokio : les appels bloquants (`virsh`, `qemu-img`, `ssh`…) d'une machine ne bloquent
//! pas les autres. Les machines sont prises dans l'ordre. Au premier échec, un signal
//! d'annulation est envoyé : les machines pas encore commencées ne démarrent pas, celles
//! en cours s'arrêtent au prochain point d'attente.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::output;

/// Futur d'une machine ; peut emprunter l'environnement de l'appelant (`'env`).
pub type JobFuture<'env> = Pin<Box<dyn Future<Output = Result<()>> + 'env>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Done,
    /// Cause de l'échec (chaîne d'erreur complète).
    Failed(String),
    /// Interrompue par l'échec d'une autre machine.
    Cancelled,
    /// Jamais commencée (annulation avant son tour).
    NotStarted,
}

#[derive(Debug, Clone)]
pub struct JobReport {
    pub label: String,
    pub status: JobStatus,
    pub elapsed: Duration,
}

impl JobReport {
    /// La machine a commencé (ses effets éventuels sont à défaire en cas d'échec global).
    pub fn started(&self) -> bool {
        self.status != JobStatus::NotStarted
    }
}

/// Lance `run(i)` pour chaque libellé `labels[i]`, au plus `parallel` à la fois.
/// Avec `prefix`, chaque ligne émise par une machine (`verr!` / `vout!`) est préfixée
/// par son libellé aligné. Rapports dans l'ordre des libellés.
pub fn run_jobs<'env, F>(labels: &[String], parallel: usize, prefix: bool, run: F) -> Vec<JobReport>
where
    F: Fn(usize) -> JobFuture<'env> + Sync,
{
    let count = labels.len();
    let workers = parallel.clamp(1, count.max(1));
    let width = labels.iter().map(String::len).max().unwrap_or(0);
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let reports: Mutex<Vec<Option<JobReport>>> = Mutex::new(vec![None; count]);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (run, next, failed, cancel_tx, reports) =
                (&run, &next, &failed, &cancel_tx, &reports);
            let mut cancel_rx = cancel_rx.clone();
            scope.spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime tokio d'un fil de création");
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= count {
                        break;
                    }
                    let label = labels[i].clone();
                    if failed.load(Ordering::SeqCst) {
                        reports.lock().unwrap()[i] = Some(JobReport {
                            label,
                            status: JobStatus::NotStarted,
                            elapsed: Duration::ZERO,
                        });
                        continue;
                    }
                    if prefix {
                        output::set_prefix(Some(format!("[{label:<width$}] ")));
                    }
                    let start = Instant::now();
                    let outcome = rt.block_on(async {
                        tokio::select! {
                            result = run(i) => Some(result),
                            () = cancelled(&mut cancel_rx) => None,
                        }
                    });
                    output::set_prefix(None);
                    let status = match outcome {
                        Some(Ok(())) => JobStatus::Done,
                        Some(Err(err)) => {
                            failed.store(true, Ordering::SeqCst);
                            let _ = cancel_tx.send(true);
                            JobStatus::Failed(format!("{err:#}"))
                        }
                        None => JobStatus::Cancelled,
                    };
                    reports.lock().unwrap()[i] = Some(JobReport {
                        label,
                        status,
                        elapsed: start.elapsed(),
                    });
                }
            });
        }
    });

    reports
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("chaque machine a un rapport"))
        .collect()
}

/// Se termine quand l'annulation est signalée (jamais si l'émetteur disparaît).
async fn cancelled(rx: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Durée lisible pour le récapitulatif (`48 s`, `2 min 05 s`).
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    /// Chaque « machine » attend `ms` puis réussit, sauf `fail` qui échoue.
    fn job<'a>(ms: u64, fail: bool) -> JobFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            if fail {
                anyhow::bail!("échec simulé");
            }
            Ok(())
        })
    }

    #[test]
    fn runs_in_parallel_up_to_the_limit() {
        let names = labels(&["a", "b", "c"]);
        let start = Instant::now();
        let reports = run_jobs(&names, 3, true, |_| job(300, false));
        let parallel = start.elapsed();
        assert!(reports.iter().all(|r| r.status == JobStatus::Done));
        assert!(parallel < Duration::from_millis(700), "{parallel:?}");

        let start = Instant::now();
        let reports = run_jobs(&names, 1, false, |_| job(150, false));
        assert!(reports.iter().all(|r| r.status == JobStatus::Done));
        assert!(start.elapsed() >= Duration::from_millis(450), "séquentiel");
        let order: Vec<_> = reports.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn failure_cancels_running_jobs_and_skips_the_rest() {
        let names = labels(&["a", "b", "c", "d"]);
        let start = Instant::now();
        // a et c durent 5 s ; b échoue après 100 ms ; d attend une place libre.
        let reports = run_jobs(&names, 3, true, |i| match i {
            1 => job(100, true),
            3 => job(10, false),
            _ => job(5_000, false),
        });
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        let status: Vec<_> = reports.iter().map(|r| r.status.clone()).collect();
        assert_eq!(
            status,
            vec![
                JobStatus::Cancelled,
                JobStatus::Failed("échec simulé".into()),
                JobStatus::Cancelled,
                JobStatus::NotStarted,
            ]
        );
        assert!(reports[0].started() && reports[1].started() && !reports[3].started());
    }

    #[test]
    fn sequential_failure_stops_before_next_jobs() {
        let names = labels(&["a", "b", "c"]);
        let reports = run_jobs(&names, 1, false, |i| job(10, i == 0));
        assert!(matches!(reports[0].status, JobStatus::Failed(_)));
        assert_eq!(reports[1].status, JobStatus::NotStarted);
        assert_eq!(reports[2].status, JobStatus::NotStarted);
    }

    #[test]
    fn durations_are_readable() {
        assert_eq!(format_duration(Duration::from_secs(48)), "48 s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2 min 05 s");
    }
}
