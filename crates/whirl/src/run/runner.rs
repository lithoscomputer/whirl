//! Run orchestration (SPEC 12, 13): input dedup, artifact-directory
//! planning, the worker pool with one reused shim process per slot,
//! fail-fast scheduling, and assembly of the run report.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{path, thread};

use crate::lang::ast::File;
use crate::report::model::{FileReport, RunReport, SETUP_ENTRY, Status};
use crate::run::artifacts::{self, ArtifactsError, Flow};
use crate::run::flow::{FlowFlags, FlowRun, Overrides, run_flow};
use crate::run::shim::{ShimClient, ShimError, ShimLaunch, resolve_launch};

/// Everything a run needs beyond its parsed files.
#[derive(Clone, Debug)]
pub struct RunSettings {
    /// Worker slots (SPEC 12); `None` uses the logical CPU count.
    pub jobs:          Option<usize>,
    pub fail_fast:     bool,
    /// The `--artifacts` directory (possibly relative).
    pub artifacts_dir: PathBuf,
    pub flags:         FlowFlags,
    pub overrides:     Overrides,
    /// `--variables-file` entries then `--var` flags, in order.
    pub base_vars:     Vec<(String, String)>,
}

/// A failure before any flow runs.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// A runtime error (exit 3).
    #[error(transparent)]
    Artifacts(#[from] ArtifactsError),
    /// A runtime error (exit 3).
    #[error(transparent)]
    Shim(#[from] ShimError),
    /// A usage error (exit 4): `--save-storage` needs a single file.
    #[error("--save-storage requires a single input file, got {count}")]
    SaveStorageManyFiles { count: usize },
}

/// One scheduled flow: its parsed file and its artifact directories.
struct FlowJob {
    file:       File,
    canonical:  PathBuf,
    report_dir: PathBuf,
    abs_dir:    PathBuf,
}

/// Runs every input file and returns the run report. `files` is the
/// expanded, parsed input list in command-line order; duplicates (by
/// canonical path) run once (SPEC 14).
pub async fn run_files(files: &[File], settings: &RunSettings) -> Result<RunReport, RunnerError> {
    let started = Instant::now();
    let inputs: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    let cwd = PathBuf::from(".");
    let flows = artifacts::plan_flows(&settings.artifacts_dir, &cwd, &inputs)?;
    if settings.flags.save_storage.is_some() && flows.len() > 1 {
        return Err(RunnerError::SaveStorageManyFiles { count: flows.len() });
    }
    let launch = resolve_launch()?;

    let jobs = build_jobs(files, flows)?;
    let job_count = jobs.len();
    let workers = settings
        .jobs
        .unwrap_or_else(default_jobs)
        .clamp(1, job_count.max(1));

    let queue = Arc::new(Mutex::new((0..job_count).collect::<VecDeque<usize>>()));
    let stop = Arc::new(AtomicBool::new(false));
    let results = Arc::new(Mutex::new(vec![None::<FileReport>; job_count]));
    let jobs: Arc<[FlowJob]> = Arc::from(jobs);
    let settings = Arc::new(settings.clone());

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        handles.push(tokio::spawn(run_worker(
            Arc::clone(&jobs),
            Arc::clone(&queue),
            Arc::clone(&stop),
            Arc::clone(&results),
            Arc::clone(&settings),
            launch.clone(),
        )));
    }
    for handle in handles {
        // A worker panic is a bug; surface it instead of hanging the run.
        handle.await.expect("a worker task does not panic");
    }

    let mut collected = results
        .lock()
        .expect("no worker holds the results lock after joining");
    let files = collected.iter_mut().filter_map(Option::take).collect();
    Ok(RunReport {
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        files,
    })
}

/// The default worker count: the logical CPU count (SPEC 12).
fn default_jobs() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Pairs each planned flow with its parsed file and absolute artifact
/// directory.
fn build_jobs(files: &[File], flows: Vec<Flow>) -> Result<Vec<FlowJob>, RunnerError> {
    flows
        .into_iter()
        .map(|flow| {
            let file = files
                .iter()
                .find(|file| file.path == flow.input)
                .expect("every planned flow came from an input file")
                .clone();
            let abs_dir = path::absolute(&flow.dir).map_err(|source| {
                RunnerError::Artifacts(ArtifactsError::Canonicalize {
                    path: flow.dir.clone(),
                    source,
                })
            })?;
            Ok(FlowJob {
                file,
                canonical: flow.canonical,
                report_dir: flow.dir,
                abs_dir,
            })
        })
        .collect()
}

/// One worker slot: pulls flows from the queue and runs each on its own
/// shim process, reused across files and respawned after a death.
async fn run_worker(
    jobs: Arc<[FlowJob]>,
    queue: Arc<Mutex<VecDeque<usize>>>,
    stop: Arc<AtomicBool>,
    results: Arc<Mutex<Vec<Option<FileReport>>>>,
    settings: Arc<RunSettings>,
    launch: ShimLaunch,
) {
    let mut client: Option<ShimClient> = None;
    loop {
        if settings.fail_fast && stop.load(Ordering::SeqCst) {
            break;
        }
        let index = {
            let mut queue = queue
                .lock()
                .expect("queue users do not panic while holding the lock");
            queue.pop_front()
        };
        let Some(index) = index else {
            break;
        };
        let job = &jobs[index];
        let report = run_job(job, &settings, &launch, &mut client).await;
        if report.status != Status::Passed {
            stop.store(true, Ordering::SeqCst);
        }
        results
            .lock()
            .expect("result writers do not panic while holding the lock")[index] = Some(report);
    }
    if let Some(client) = client {
        // Best-effort clean shutdown of the worker's shim process.
        let _ = client.shutdown().await;
    }
}

/// Runs one flow on the worker's shim process, spawning or respawning
/// the process when needed. A spawn or `hello` failure reports as a
/// runtime error for this file only; the next file retries.
async fn run_job(
    job: &FlowJob,
    settings: &RunSettings,
    launch: &ShimLaunch,
    client: &mut Option<ShimClient>,
) -> FileReport {
    if client.as_ref().is_none_or(|client| !client.is_alive()) {
        match spawn_client(launch).await {
            Ok(fresh) => *client = Some(fresh),
            Err(error) => {
                *client = None;
                return setup_error_report(job, &error.to_string());
            }
        }
    }
    let client = client
        .as_mut()
        .expect("the worker's client was just spawned or verified alive");
    let run = FlowRun {
        file:       &job.file,
        canonical:  &job.canonical,
        report_dir: &job.report_dir,
        abs_dir:    &job.abs_dir,
        flags:      &settings.flags,
        overrides:  &settings.overrides,
        base_vars:  &settings.base_vars,
    };
    run_flow(&run, client).await
}

/// Spawns a shim process and completes the `hello` handshake.
async fn spawn_client(launch: &ShimLaunch) -> Result<ShimClient, ShimError> {
    let mut client = ShimClient::spawn(launch)?;
    client.hello().await?;
    Ok(client)
}

/// A file that could not start at all: a `[setup]` runtime error.
fn setup_error_report(job: &FlowJob, message: &str) -> FileReport {
    use crate::report::model::{EntryReport, StepError, StepKind, StepReport};

    FileReport {
        path:          job.file.path.to_string_lossy().into_owned(),
        status:        Status::Error,
        duration_ms:   0,
        artifacts_dir: job.report_dir.to_string_lossy().into_owned(),
        blocked_hosts: Vec::new(),
        warnings:      Vec::new(),
        artifacts:     Vec::new(),
        entries:       vec![EntryReport {
            name:        SETUP_ENTRY.to_owned(),
            line:        0,
            status:      Status::Error,
            duration_ms: 0,
            steps:       vec![StepReport {
                line:        0,
                kind:        StepKind::Action,
                text:        SETUP_ENTRY.to_owned(),
                status:      Status::Error,
                duration_ms: 0,
                error:       Some(StepError {
                    message: message.to_owned(),
                    ..StepError::default()
                }),
            }],
            captures:    Vec::new(),
            artifacts:   Vec::new(),
        }],
    }
}
