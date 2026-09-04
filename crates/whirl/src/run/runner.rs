//! Run orchestration (SPEC 12, 13): input dedup, artifact-directory
//! planning, the worker pool with one reused shim process per slot,
//! fail-fast scheduling, and assembly of the run report.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{env, fs, path, process, thread};

use crate::lang::ast::File;
use crate::report::model::{FileReport, RunReport, SETUP_ENTRY, Status};
use crate::run::artifacts::{self, ArtifactsError, Flow};
use crate::run::flow::{
    FlowFlags, FlowOutcome, FlowRun, Overrides, SetupHandoff, run_flow, setup_path_for,
};
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

/// One scheduled flow: its parsed file, its artifact directories, and
/// its place in the setup graph (SPEC 12).
struct FlowJob {
    file:            File,
    canonical:       PathBuf,
    report_dir:      PathBuf,
    abs_dir:         PathBuf,
    /// The canonical path of this file's `setup` flow, when it has one.
    setup_canonical: Option<PathBuf>,
    /// Where this flow saves its final state when other files depend on
    /// it.
    state_out:       Option<PathBuf>,
}

/// What a finished setup flow leaves for its dependents: the handoff on
/// success, or the message dependents report on failure.
enum SetupResult {
    Ready(SetupHandoff),
    Failed(String),
}

/// Runs every input file and returns the run report. `files` is the
/// expanded, parsed input list in command-line order; duplicates (by
/// canonical path) run once (SPEC 14). `setups` holds the parsed `setup`
/// flows of those files that are not inputs themselves. Setup flows run
/// first, once each, and their dependents start from the saved state
/// (SPEC 12).
pub async fn run_files(
    files: &[File],
    setups: &[File],
    settings: &RunSettings,
) -> Result<RunReport, RunnerError> {
    let started = Instant::now();
    let inputs: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    if settings.flags.save_storage.is_some() && artifacts::dedup_flows(&inputs)?.len() > 1 {
        return Err(RunnerError::SaveStorageManyFiles {
            count: artifacts::dedup_flows(&inputs)?.len(),
        });
    }
    let cwd = PathBuf::from(".");

    // Setup flows are planned first so they run first and dedup against
    // inputs that name the same file.
    let mut setup_paths: Vec<PathBuf> = Vec::new();
    for file in files {
        if let Some(path) = setup_path_for(file) {
            if !setup_paths.contains(&path) {
                setup_paths.push(path);
            }
        }
    }
    let planned: Vec<PathBuf> = setup_paths.iter().chain(inputs.iter()).cloned().collect();
    let flows = artifacts::plan_flows(&settings.artifacts_dir, &cwd, &planned)?;
    let setup_canonicals: Vec<PathBuf> = flows
        .iter()
        .take(artifacts::dedup_flows(&setup_paths)?.len())
        .map(|flow| flow.canonical.clone())
        .collect();
    let state_dir = env::temp_dir().join(format!("whirl-setup-{}", process::id()));
    let launch = resolve_launch()?;

    let all_files: Vec<&File> = files.iter().chain(setups.iter()).collect();
    let jobs = build_jobs(&all_files, &flows, &setup_canonicals, &state_dir)?;
    let (setup_jobs, main_jobs): (Vec<FlowJob>, Vec<FlowJob>) = jobs
        .into_iter()
        .partition(|job| setup_canonicals.contains(&job.canonical));
    let workers = settings
        .jobs
        .unwrap_or_else(default_jobs)
        .clamp(1, setup_jobs.len().max(main_jobs.len()).max(1));
    let stop = Arc::new(AtomicBool::new(false));
    let settings = Arc::new(settings.clone());

    let mut reports = Vec::new();
    let mut handoffs: HashMap<PathBuf, SetupResult> = HashMap::new();
    if !setup_jobs.is_empty() {
        if let Err(error) = fs::create_dir_all(&state_dir) {
            return Err(RunnerError::Artifacts(ArtifactsError::Canonicalize {
                path:   state_dir,
                source: error,
            }));
        }
        let setup_jobs: Arc<[FlowJob]> = Arc::from(setup_jobs);
        let outcomes = run_pool(
            Arc::clone(&setup_jobs),
            Arc::new(HashMap::new()),
            Arc::clone(&settings),
            &launch,
            workers,
            Arc::clone(&stop),
        )
        .await;
        for (job, outcome) in setup_jobs.iter().zip(outcomes) {
            let Some(outcome) = outcome else {
                continue;
            };
            let result = if outcome.report.status == Status::Passed {
                SetupResult::Ready(SetupHandoff {
                    storage_path: job
                        .state_out
                        .clone()
                        .expect("every setup job has a state path"),
                    captures:     outcome.captures,
                    secrets:      outcome.secrets,
                })
            } else {
                SetupResult::Failed(setup_failure_message(&job.file, &outcome.report))
            };
            handoffs.insert(job.canonical.clone(), result);
            reports.push(outcome.report);
        }
    }

    let main_jobs: Arc<[FlowJob]> = Arc::from(main_jobs);
    let outcomes = run_pool(
        Arc::clone(&main_jobs),
        Arc::new(handoffs),
        settings,
        &launch,
        workers,
        stop,
    )
    .await;
    reports.extend(outcomes.into_iter().flatten().map(|outcome| outcome.report));
    // The saved setup states hold session cookies; do not leave them
    // behind (SPEC 11).
    let _ = fs::remove_dir_all(&state_dir);

    Ok(RunReport {
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        files:       reports,
    })
}

/// The default worker count: the logical CPU count (SPEC 12).
fn default_jobs() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Pairs each planned flow with its parsed file, its absolute artifact
/// directory, and its setup relationship.
fn build_jobs(
    files: &[&File],
    flows: &[Flow],
    setup_canonicals: &[PathBuf],
    state_dir: &Path,
) -> Result<Vec<FlowJob>, RunnerError> {
    let canonical_of = |input: &Path| -> Option<PathBuf> {
        flows
            .iter()
            .find(|flow| flow.input == input)
            .map(|flow| flow.canonical.clone())
            .or_else(|| input.canonicalize().ok())
    };
    flows
        .iter()
        .map(|flow| {
            let file = files
                .iter()
                .find(|file| file.path == flow.input)
                .expect("every planned flow came from an input or setup file");
            let abs_dir = path::absolute(&flow.dir).map_err(|source| {
                RunnerError::Artifacts(ArtifactsError::Canonicalize {
                    path: flow.dir.clone(),
                    source,
                })
            })?;
            let is_setup = setup_canonicals.contains(&flow.canonical);
            let setup_canonical = setup_path_for(file).and_then(|path| canonical_of(&path));
            Ok(FlowJob {
                file: (*file).clone(),
                canonical: flow.canonical.clone(),
                report_dir: flow.dir.clone(),
                abs_dir,
                setup_canonical,
                state_out: is_setup.then(|| {
                    state_dir.join(format!("{}.json", artifacts::path_hash(&flow.canonical)))
                }),
            })
        })
        .collect()
}

/// Runs one job list through `workers` worker slots and returns each
/// job's outcome in job order (`None` when fail-fast skipped it).
async fn run_pool(
    jobs: Arc<[FlowJob]>,
    handoffs: Arc<HashMap<PathBuf, SetupResult>>,
    settings: Arc<RunSettings>,
    launch: &ShimLaunch,
    workers: usize,
    stop: Arc<AtomicBool>,
) -> Vec<Option<FlowOutcome>> {
    let job_count = jobs.len();
    if job_count == 0 {
        return Vec::new();
    }
    let queue = Arc::new(Mutex::new((0..job_count).collect::<VecDeque<usize>>()));
    let results = Arc::new(Mutex::new(
        (0..job_count)
            .map(|_| None::<FlowOutcome>)
            .collect::<Vec<_>>(),
    ));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers.min(job_count) {
        handles.push(tokio::spawn(run_worker(
            Arc::clone(&jobs),
            Arc::clone(&handoffs),
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
    collected.iter_mut().map(Option::take).collect()
}

/// One worker slot: pulls flows from the queue and runs each on its own
/// shim process, reused across files and respawned after a death.
async fn run_worker(
    jobs: Arc<[FlowJob]>,
    handoffs: Arc<HashMap<PathBuf, SetupResult>>,
    queue: Arc<Mutex<VecDeque<usize>>>,
    stop: Arc<AtomicBool>,
    results: Arc<Mutex<Vec<Option<FlowOutcome>>>>,
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
        let outcome = run_job(job, &handoffs, &settings, &launch, &mut client).await;
        if outcome.report.status != Status::Passed {
            stop.store(true, Ordering::SeqCst);
        }
        results
            .lock()
            .expect("result writers do not panic while holding the lock")[index] = Some(outcome);
    }
    if let Some(client) = client {
        // Best-effort clean shutdown of the worker's shim process.
        let _ = client.shutdown().await;
    }
}

/// Runs one flow on the worker's shim process, spawning or respawning
/// the process when needed. A spawn or `hello` failure reports as a
/// runtime error for this file only; the next file retries. A file whose
/// setup flow failed does not start; it reports that failure as its own
/// `[setup]` case (SPEC 12).
async fn run_job(
    job: &FlowJob,
    handoffs: &HashMap<PathBuf, SetupResult>,
    settings: &RunSettings,
    launch: &ShimLaunch,
    client: &mut Option<ShimClient>,
) -> FlowOutcome {
    let setup = match job.setup_canonical.as_ref().map(|path| handoffs.get(path)) {
        None => None,
        Some(Some(SetupResult::Ready(handoff))) => Some(handoff),
        Some(Some(SetupResult::Failed(message))) => {
            return synthetic_outcome(job, Status::Failed, message);
        }
        Some(None) => {
            return synthetic_outcome(job, Status::Failed, "the setup flow did not run");
        }
    };
    if client.as_ref().is_none_or(|client| !client.is_alive()) {
        match spawn_client(launch).await {
            Ok(fresh) => *client = Some(fresh),
            Err(error) => {
                *client = None;
                return synthetic_outcome(job, Status::Error, &error.to_string());
            }
        }
    }
    let client = client
        .as_mut()
        .expect("the worker's client was just spawned or verified alive");
    let run = FlowRun {
        file: &job.file,
        canonical: &job.canonical,
        report_dir: &job.report_dir,
        abs_dir: &job.abs_dir,
        flags: &settings.flags,
        overrides: &settings.overrides,
        base_vars: &settings.base_vars,
        setup,
        state_out: job.state_out.as_deref(),
    };
    run_flow(&run, client).await
}

/// The message a dependent reports when its setup flow failed: the setup
/// file and the first failing step's error.
fn setup_failure_message(setup: &File, report: &FileReport) -> String {
    let detail = report
        .entries
        .iter()
        .flat_map(|entry| &entry.steps)
        .find_map(|step| step.error.as_ref().map(|error| error.message.clone()))
        .unwrap_or_else(|| format!("{:?}", report.status).to_lowercase());
    format!("setup flow '{}' failed: {detail}", setup.path.display())
}

/// Spawns a shim process and completes the `hello` handshake.
async fn spawn_client(launch: &ShimLaunch) -> Result<ShimClient, ShimError> {
    let mut client = ShimClient::spawn(launch)?;
    client.hello().await?;
    Ok(client)
}

/// A file that could not start at all: a synthetic `[setup]` case, a
/// runtime error for a dead shim or a failure for a failed setup flow.
fn synthetic_outcome(job: &FlowJob, status: Status, message: &str) -> FlowOutcome {
    use crate::report::model::{EntryReport, StepError, StepKind, StepReport};

    FlowOutcome {
        report:   FileReport {
            runtime: None,
            path: job.file.path.to_string_lossy().into_owned(),
            status,
            duration_ms: 0,
            artifacts_dir: job.report_dir.to_string_lossy().into_owned(),
            blocked_hosts: Vec::new(),
            warnings: Vec::new(),
            artifacts: Vec::new(),
            entries: vec![EntryReport {
                name: SETUP_ENTRY.to_owned(),
                line: 0,
                status,
                duration_ms: 0,
                steps: vec![StepReport {
                    line: 0,
                    kind: StepKind::Action,
                    text: SETUP_ENTRY.to_owned(),
                    status,
                    duration_ms: 0,
                    error: Some(StepError {
                        code: "setup-failed".to_owned(),
                        message: message.to_owned(),
                        ..StepError::default()
                    }),
                }],
                captures: Vec::new(),
                artifacts: Vec::new(),
            }],
        },
        captures: Vec::new(),
        secrets:  Vec::new(),
    }
}
