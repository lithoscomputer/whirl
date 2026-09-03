//! The shim process client: spawning `<node> <shim-js>`, the JSON-Lines
//! request/response protocol of `docs/engineering/shim-protocol.md`, the
//! external per-step watchdog, and clean shutdown.
//!
//! One [`ShimClient`] owns one shim child process (one worker slot). A
//! background task reads stdout and dispatches responses to their
//! requests by id, so responses may arrive out of request order; stderr
//! is captured into a bounded tail for runtime-error reports.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, io};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};

/// Environment variable naming the built shim entry (protocol section 8).
pub const SHIM_JS_ENV: &str = "WHIRL_SHIM_JS";
/// Environment variable naming the node executable (protocol section 8).
pub const NODE_ENV: &str = "WHIRL_NODE";
/// The default node executable when only [`SHIM_JS_ENV`] is set.
pub const DEFAULT_NODE: &str = "node";

/// Whirl's directory under the platform data dir (`dirs::data_dir()`).
pub const DATA_DIR_NAME: &str = "whirl";
/// Environment override for the Whirl data directory. Used by tests to
/// point shim resolution and `whirl install` at a scratch directory; the
/// value replaces `dirs::data_dir()/whirl` entirely.
pub const DATA_DIR_ENV: &str = "WHIRL_DATA_DIR";
/// The bundled node executable, relative to the data dir.
pub const BUNDLE_NODE: &str = "bundle/node/bin/node";
/// The bundled shim entry, relative to the data dir.
pub const BUNDLE_SHIM_JS: &str = "bundle/shim/index.js";

/// How long past a step's `timeoutMs` the external watchdog waits before
/// it sends `cancelFlow`, and then how long it waits for the
/// `cancelFlow` reply before killing the process (protocol section 5).
pub const WATCHDOG_GRACE: Duration = Duration::from_secs(2);

/// How long `shutdown` waits for the shim to acknowledge and exit
/// before killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Kept bytes of the shim's most recent stderr output.
const STDERR_TAIL_LIMIT: usize = 8 * 1024;

/// How to launch the shim: the node executable and the shim entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShimLaunch {
    pub node:    PathBuf,
    pub shim_js: PathBuf,
}

/// A shim process or protocol failure. Protocol-level step errors are
/// not in here; they surface as [`StepOutcome::ShimError`].
#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error(
        "no browser shim found: set {SHIM_JS_ENV} or run `whirl install` to provision the bundle"
    )]
    NotInstalled,
    #[error("cannot launch the shim ({node} {shim_js}): {source}", node = launch.node.display(), shim_js = launch.shim_js.display())]
    Spawn {
        launch: ShimLaunch,
        source: io::Error,
    },
    #[error("the shim process died unexpectedly; stderr:\n{stderr_tail}")]
    ProcessDied { stderr_tail: String },
    #[error("shim error ({kind}): {message}", kind = .0.kind, message = .0.message)]
    Shim(ErrorObject),
}

/// The wire error object (protocol section 2).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorObject {
    pub kind:       String,
    pub message:    String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected:   Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual:     Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<String>>,
}

/// Resolves how to launch the shim (protocol section 8): the
/// `WHIRL_SHIM_JS`/`WHIRL_NODE` environment, then the installed bundle
/// under the platform data dir. Neither present is a runtime error
/// naming `whirl install`.
pub fn resolve_launch() -> Result<ShimLaunch, ShimError> {
    resolve_launch_from(
        env::var_os(SHIM_JS_ENV).map(PathBuf::from),
        env::var_os(NODE_ENV).map(PathBuf::from),
        whirl_data_dir(),
    )
}

/// The Whirl data directory: the [`DATA_DIR_ENV`] override when set,
/// otherwise `dirs::data_dir()/whirl`. `None` only when the platform has
/// no data directory and no override is set.
pub fn whirl_data_dir() -> Option<PathBuf> {
    data_dir_from(
        env::var_os(DATA_DIR_ENV).map(PathBuf::from),
        dirs::data_dir(),
    )
}

fn data_dir_from(env_override: Option<PathBuf>, platform: Option<PathBuf>) -> Option<PathBuf> {
    env_override.or_else(|| platform.map(|dir| dir.join(DATA_DIR_NAME)))
}

fn resolve_launch_from(
    env_shim_js: Option<PathBuf>,
    env_node: Option<PathBuf>,
    data_dir: Option<PathBuf>,
) -> Result<ShimLaunch, ShimError> {
    if let Some(shim_js) = env_shim_js {
        return Ok(ShimLaunch {
            node: env_node.unwrap_or_else(|| PathBuf::from(DEFAULT_NODE)),
            shim_js,
        });
    }
    if let Some(data_dir) = data_dir {
        let node = data_dir.join(BUNDLE_NODE);
        let shim_js = data_dir.join(BUNDLE_SHIM_JS);
        if node.is_file() && shim_js.is_file() {
            return Ok(ShimLaunch { node, shim_js });
        }
    }
    Err(ShimError::NotInstalled)
}

/// `hello` result (protocol section 3).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub protocol:           u64,
    pub playwright_version: String,
}

/// `startFlow` viewport params.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ViewportParams {
    pub width:  u64,
    pub height: u64,
}

/// `startFlow` video params: record into `temp_dir`, move the recording
/// to `final_path` at `endFlow`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoParams {
    pub temp_dir:   String,
    pub final_path: String,
}

/// `startFlow` params (protocol section 3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartFlowParams {
    pub browser:            String,
    pub headed:             bool,
    pub viewport:           ViewportParams,
    pub storage_state_path: Option<String>,
    pub dialogs:            String,
    pub allow_hosts:        Option<Vec<String>>,
    pub nav_timeout_ms:     u64,
    pub video:              Option<VideoParams>,
    pub har_path:           Option<String>,
    pub trace:              bool,
}

/// `endFlow` params (protocol section 3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndFlowParams {
    pub save_storage_path: Option<String>,
    pub trace_path:        Option<String>,
}

/// `endFlow` result (protocol section 3).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EndFlowResult {
    pub blocked_hosts: Vec<String>,
    pub video_path:    Option<String>,
}

/// A step command's own params (protocol section 4). Locator, PAGE
/// expectation, assert spec, and capture source/filter bodies come from
/// [`crate::lang::wire`] as JSON values.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "cmd", content = "params", rename_all = "camelCase")]
pub enum StepCommand {
    Visit {
        url: String,
    },
    Click {
        locator: Json,
    },
    Dblclick {
        locator: Json,
    },
    Fill {
        locator: Json,
        value:   String,
    },
    Type {
        locator: Json,
        text:    String,
    },
    Press {
        locator: Option<Json>,
        key:     String,
    },
    /// `CHECK` / `UNCHECK`.
    Checkbox {
        locator: Json,
        checked: bool,
    },
    SelectOption {
        locator: Json,
        label:   String,
    },
    Hover {
        locator: Json,
    },
    Upload {
        locator: Json,
        path:    String,
    },
    Screenshot {
        path: String,
    },
    #[serde(rename_all = "camelCase")]
    Snapshot {
        baseline_path: String,
        actual_path:   String,
        diff_path:     String,
        update:        bool,
    },
    EvalAction {
        script: String,
    },
    Page {
        expect: Json,
    },
    Assert {
        spec: Json,
    },
    Capture {
        source: Json,
        filter: Json,
    },
}

/// One step request: the command plus the common `timeoutMs` and
/// `title` params (protocol section 4). `title` is the rendered,
/// secret-masked step text.
#[derive(Clone, Debug, PartialEq)]
pub struct StepRequest {
    pub command:    StepCommand,
    pub timeout_ms: u64,
    pub title:      String,
}

/// `capture` result (protocol section 4).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CaptureResult {
    pub value: String,
}

/// `snapshot` result with `update: true` (protocol section 4).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SnapshotUpdatedResult {
    pub updated: bool,
}

/// The outcome of one step run under the external watchdog.
#[derive(Debug)]
pub enum StepOutcome {
    /// The shim answered `ok: true`; the raw result object. `capture`
    /// deserializes to [`CaptureResult`], `snapshot` with `update` to
    /// [`SnapshotUpdatedResult`]; other steps return `{}`.
    Ok(Json),
    /// The shim answered `ok: false` with a protocol error object.
    ShimError(ErrorObject),
    /// The external watchdog fired. With `process_killed` false the
    /// shim answered `cancelFlow` and is ready for the next `startFlow`;
    /// with true it was unresponsive, was killed with SIGKILL, and the
    /// owner must respawn the client.
    StepTimeout { process_killed: bool },
    /// The process died before answering; the owner must respawn.
    ProcessDied { stderr_tail: String },
}

/// A response payload dispatched to its awaiting request.
type RawResponse = Result<Json, ErrorObject>;

/// Requests waiting for their response, by id. The reader task drops
/// the whole map when the shim's stdout closes, which wakes every
/// waiter with a channel error (process died).
type PendingMap = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<RawResponse>>>>>;

/// An async client for one shim process (one worker slot). The owner
/// respawns a fresh client when [`ShimClient::is_alive`] turns false.
#[derive(Debug)]
pub struct ShimClient {
    child:          Child,
    stdin:          ChildStdin,
    next_id:        u64,
    pending:        PendingMap,
    stderr_tail:    Arc<Mutex<Vec<u8>>>,
    reader_task:    JoinHandle<()>,
    stderr_task:    JoinHandle<()>,
    watchdog_grace: Duration,
    alive:          bool,
}

impl ShimClient {
    /// Spawns `<node> <shim-js>` with piped stdio and starts the
    /// background stdout reader and stderr capture. The caller sends
    /// `hello` next (protocol section 3).
    pub fn spawn(launch: &ShimLaunch) -> Result<Self, ShimError> {
        let mut child = Command::new(&launch.node)
            .arg(&launch.shim_js)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ShimError::Spawn {
                launch: launch.clone(),
                source,
            })?;
        let stdin = child
            .stdin
            .take()
            .expect("stdin was configured as piped at spawn");
        let stdout = child
            .stdout
            .take()
            .expect("stdout was configured as piped at spawn");
        let stderr = child
            .stderr
            .take()
            .expect("stderr was configured as piped at spawn");

        let pending: PendingMap = Arc::new(Mutex::new(Some(HashMap::new())));
        let reader_task = tokio::spawn(read_responses(stdout, Arc::clone(&pending)));
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(read_stderr(stderr, Arc::clone(&stderr_tail)));

        Ok(Self {
            child,
            stdin,
            next_id: 1,
            pending,
            stderr_tail,
            reader_task,
            stderr_task,
            watchdog_grace: WATCHDOG_GRACE,
            alive: true,
        })
    }

    /// True while the process is believed to be running and usable.
    /// After a kill (unresponsive shim, failed shutdown) or an observed
    /// process death this turns false and the owner must respawn.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Overrides the watchdog grace period (default
    /// [`WATCHDOG_GRACE`]).
    pub fn set_watchdog_grace(&mut self, grace: Duration) {
        self.watchdog_grace = grace;
    }

    /// The most recent stderr output of the shim process (bounded).
    pub fn stderr_tail(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .expect("the stderr capture task does not panic while holding the lock");
        String::from_utf8_lossy(&tail).into_owned()
    }

    /// Registers a pending request and writes its frame. Returns the
    /// receiver for the response.
    async fn send_request(
        &mut self,
        cmd: &str,
        params: Json,
    ) -> Result<oneshot::Receiver<RawResponse>, ShimError> {
        let id = self.next_id;
        self.next_id += 1;
        let (sender, receiver) = oneshot::channel();
        let registered = {
            let mut pending = self
                .pending
                .lock()
                .expect("the response reader task does not panic while holding the lock");
            match pending.as_mut() {
                Some(map) => {
                    map.insert(id, sender);
                    true
                }
                None => false,
            }
        };
        if !registered {
            return Err(self.process_died());
        }
        let frame = serde_json::to_string(&serde_json::json!({
            "id": id,
            "cmd": cmd,
            "params": params,
        }))
        .expect("a request frame of JSON values always serializes");
        let write = async {
            self.stdin.write_all(frame.as_bytes()).await?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await
        };
        if write.await.is_err() {
            return Err(self.process_died());
        }
        Ok(receiver)
    }

    /// Marks the process dead and builds the error carrying the stderr
    /// tail.
    fn process_died(&mut self) -> ShimError {
        self.alive = false;
        ShimError::ProcessDied {
            stderr_tail: self.stderr_tail(),
        }
    }

    /// Sends a lifecycle request and awaits its response with no
    /// watchdog: `hello`, `startFlow`, `endFlow`, `cancelFlow`.
    async fn request<T: DeserializeOwned>(
        &mut self,
        cmd: &str,
        params: Json,
    ) -> Result<T, ShimError> {
        let receiver = self.send_request(cmd, params).await?;
        match receiver.await {
            Ok(Ok(result)) => Ok(serde_json::from_value(result).map_err(|error| {
                ShimError::Shim(ErrorObject {
                    kind:       "internal".to_owned(),
                    message:    format!("malformed {cmd} result: {error}"),
                    expected:   None,
                    actual:     None,
                    candidates: None,
                })
            })?),
            Ok(Err(error)) => Err(ShimError::Shim(error)),
            Err(_) => Err(self.process_died()),
        }
    }

    /// `hello` (protocol section 3): sent once after spawn.
    pub async fn hello(&mut self) -> Result<HelloResult, ShimError> {
        self.request("hello", serde_json::json!({})).await
    }

    /// `startFlow` (protocol section 3): creates the browser context
    /// and page for one flow.
    pub async fn start_flow(&mut self, params: &StartFlowParams) -> Result<(), ShimError> {
        let params = serde_json::to_value(params).expect("startFlow params always serialize");
        self.request::<Json>("startFlow", params).await?;
        Ok(())
    }

    /// `endFlow` (protocol section 3): ends the flow and closes the
    /// context.
    pub async fn end_flow(&mut self, params: &EndFlowParams) -> Result<EndFlowResult, ShimError> {
        let params = serde_json::to_value(params).expect("endFlow params always serialize");
        self.request("endFlow", params).await
    }
}

/// One wire response line (protocol section 2).
#[derive(Debug, Deserialize)]
struct ResponseFrame {
    id:     u64,
    ok:     bool,
    result: Option<Json>,
    error:  Option<ErrorObject>,
}

/// The background stdout reader: dispatches each response line to its
/// pending request by id. On EOF or an unreadable line it drops the
/// pending map, which wakes every current and future waiter with a
/// process-died error.
async fn read_responses(stdout: ChildStdout, pending: PendingMap) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<ResponseFrame>(&line) else {
            // A malformed frame means the stream can no longer be
            // trusted; treat it like a closed pipe.
            break;
        };
        let sender = {
            let mut pending = pending
                .lock()
                .expect("shim client request paths do not panic while holding the lock");
            pending.as_mut().and_then(|map| map.remove(&frame.id))
        };
        let Some(sender) = sender else {
            // A response for an abandoned request (for example a step
            // cancelled by the watchdog); drop it.
            continue;
        };
        let response = if frame.ok {
            Ok(frame.result.unwrap_or(Json::Null))
        } else {
            Err(frame.error.unwrap_or_else(|| ErrorObject {
                kind:       "internal".to_owned(),
                message:    "the shim reported failure without an error object".to_owned(),
                expected:   None,
                actual:     None,
                candidates: None,
            }))
        };
        let _ = sender.send(response);
    }
    let mut pending = pending
        .lock()
        .expect("shim client request paths do not panic while holding the lock");
    *pending = None;
}

/// The background stderr capture: keeps the most recent
/// [`STDERR_TAIL_LIMIT`] bytes for runtime-error reports.
async fn read_stderr(stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    let mut stderr = stderr;
    let mut chunk = [0_u8; 1024];
    loop {
        let read = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let mut tail = tail
            .lock()
            .expect("stderr tail readers do not panic while holding the lock");
        tail.extend_from_slice(&chunk[..read]);
        if tail.len() > STDERR_TAIL_LIMIT {
            let excess = tail.len() - STDERR_TAIL_LIMIT;
            tail.drain(..excess);
        }
    }
}

impl ShimClient {
    /// `cancelFlow` (protocol section 6): the out-of-band abort. The
    /// shim force-closes the page and context; the in-flight step, if
    /// any, fails with error kind `cancelled`.
    pub async fn cancel_flow(&mut self) -> Result<(), ShimError> {
        self.request::<Json>("cancelFlow", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Runs one step command under the external watchdog (protocol
    /// section 5): the shim gets `timeoutMs`; Rust waits `timeoutMs`
    /// plus the grace period, then sends `cancelFlow`; if `cancelFlow`
    /// gets no reply within another grace period, the process is killed
    /// with SIGKILL and reported dead so the owner can respawn.
    pub async fn run_step(&mut self, step: &StepRequest) -> StepOutcome {
        let (cmd, mut params) = step_frame(&step.command);
        params.insert("timeoutMs".to_owned(), Json::from(step.timeout_ms));
        params.insert("title".to_owned(), Json::from(step.title.clone()));
        let Ok(receiver) = self.send_request(&cmd, Json::Object(params)).await else {
            return self.died_outcome();
        };
        let deadline =
            Instant::now() + Duration::from_millis(step.timeout_ms) + self.watchdog_grace;
        match timeout_at(deadline, receiver).await {
            Ok(Ok(Ok(result))) => StepOutcome::Ok(result),
            Ok(Ok(Err(error))) => StepOutcome::ShimError(error),
            Ok(Err(_)) => self.died_outcome(),
            Err(_) => self.watchdog_cancel().await,
        }
    }

    /// Marks the client dead and builds the [`StepOutcome::ProcessDied`]
    /// outcome carrying the stderr tail.
    fn died_outcome(&mut self) -> StepOutcome {
        self.alive = false;
        StepOutcome::ProcessDied {
            stderr_tail: self.stderr_tail(),
        }
    }

    /// The watchdog fired: cancel the flow, and kill the process when
    /// the shim does not answer `cancelFlow` within the grace period.
    async fn watchdog_cancel(&mut self) -> StepOutcome {
        let grace = self.watchdog_grace;
        let cancelled = match timeout(grace, self.cancel_flow()).await {
            Ok(Ok(())) => true,
            Ok(Err(_)) | Err(_) => false,
        };
        if cancelled {
            return StepOutcome::StepTimeout {
                process_killed: false,
            };
        }
        self.kill().await;
        StepOutcome::StepTimeout {
            process_killed: true,
        }
    }

    /// Kills the shim process with SIGKILL and marks the client dead.
    /// The owner respawns a replacement client for the worker slot.
    pub async fn kill(&mut self) {
        self.alive = false;
        let _ = self.child.kill().await;
    }

    /// Clean shutdown (protocol section 3): send `shutdown`, wait a
    /// bounded time for the reply and process exit, then kill.
    pub async fn shutdown(mut self) -> Result<(), ShimError> {
        let acknowledged = match timeout(
            SHUTDOWN_GRACE,
            self.request::<Json>("shutdown", serde_json::json!({})),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(_)) | Err(_) => false,
        };
        let exited = acknowledged
            && timeout(SHUTDOWN_GRACE, self.child.wait())
                .await
                .is_ok_and(|status| status.is_ok());
        self.alive = false;
        if !exited {
            let _ = self.child.kill().await;
            return Err(ShimError::ProcessDied {
                stderr_tail: self.stderr_tail(),
            });
        }
        // The reader tasks end on their own at pipe EOF.
        let _ = self.reader_task.await;
        let _ = self.stderr_task.await;
        Ok(())
    }
}

/// Splits a [`StepCommand`] into its wire `cmd` name and params object,
/// ready for the common `timeoutMs` and `title` params to be merged in.
fn step_frame(command: &StepCommand) -> (String, serde_json::Map<String, Json>) {
    let frame = serde_json::to_value(command).expect("step commands always serialize");
    let Json::Object(mut frame) = frame else {
        unreachable!("an adjacently tagged enum serializes to an object");
    };
    let Some(Json::String(cmd)) = frame.remove("cmd") else {
        unreachable!("an adjacently tagged enum carries its cmd tag");
    };
    let params = match frame.remove("params") {
        Some(Json::Object(params)) => params,
        _ => serde_json::Map::new(),
    };
    (cmd, params)
}

#[cfg(test)]
mod tests {
    use std::{fs, process};

    use super::*;

    #[test]
    fn the_env_shim_wins_with_the_default_node() {
        let launch = resolve_launch_from(Some(PathBuf::from("/dev/shim.js")), None, None)
            .expect("env shim resolves");
        assert_eq!(launch, ShimLaunch {
            node:    PathBuf::from(DEFAULT_NODE),
            shim_js: PathBuf::from("/dev/shim.js"),
        });
    }

    #[test]
    fn the_env_node_overrides_the_default() {
        let launch = resolve_launch_from(
            Some(PathBuf::from("/dev/shim.js")),
            Some(PathBuf::from("/dev/node")),
            None,
        )
        .expect("env shim resolves");
        assert_eq!(launch.node, PathBuf::from("/dev/node"));
    }

    #[test]
    fn the_installed_bundle_is_the_fallback() {
        let data_dir = env::temp_dir().join(format!("whirl-shim-resolve-test-{}", process::id()));
        let node = data_dir.join(BUNDLE_NODE);
        let shim_js = data_dir.join(BUNDLE_SHIM_JS);
        for path in [&node, &shim_js] {
            let parent = path.parent().expect("bundle paths have parents");
            fs::create_dir_all(parent).expect("temp dirs should be creatable");
            fs::write(path, "").expect("temp files should be writable");
        }
        let launch =
            resolve_launch_from(None, None, Some(data_dir.clone())).expect("the bundle resolves");
        assert_eq!(launch, ShimLaunch { node, shim_js });
        fs::remove_dir_all(&data_dir).expect("temp dirs should be removable");
    }

    #[test]
    fn no_shim_anywhere_names_whirl_install() {
        let missing = env::temp_dir().join("whirl-shim-resolve-test-missing");
        let error = resolve_launch_from(None, None, Some(missing)).expect_err("nothing resolves");
        assert!(matches!(error, ShimError::NotInstalled));
        assert!(error.to_string().contains("whirl install"));
    }

    /// The wire `cmd` name and params of one step command.
    fn frame_of(command: &StepCommand) -> (String, Json) {
        let (cmd, params) = step_frame(command);
        (cmd, Json::Object(params))
    }

    #[test]
    fn step_commands_serialize_to_their_wire_names() {
        let locator = serde_json::json!([{"type": "testid", "id": "x"}]);
        let cases = [
            (
                StepCommand::Visit {
                    url: "https://example.com/".to_owned(),
                },
                "visit",
                serde_json::json!({"url": "https://example.com/"}),
            ),
            (
                StepCommand::Click {
                    locator: locator.clone(),
                },
                "click",
                serde_json::json!({"locator": locator}),
            ),
            (
                StepCommand::Dblclick {
                    locator: locator.clone(),
                },
                "dblclick",
                serde_json::json!({"locator": locator}),
            ),
            (
                StepCommand::Fill {
                    locator: locator.clone(),
                    value:   "text".to_owned(),
                },
                "fill",
                serde_json::json!({"locator": locator, "value": "text"}),
            ),
            (
                StepCommand::Type {
                    locator: locator.clone(),
                    text:    "424242".to_owned(),
                },
                "type",
                serde_json::json!({"locator": locator, "text": "424242"}),
            ),
            (
                StepCommand::Press {
                    locator: None,
                    key:     "Enter".to_owned(),
                },
                "press",
                serde_json::json!({"locator": null, "key": "Enter"}),
            ),
            (
                StepCommand::Checkbox {
                    locator: locator.clone(),
                    checked: true,
                },
                "checkbox",
                serde_json::json!({"locator": locator, "checked": true}),
            ),
            (
                StepCommand::SelectOption {
                    locator: locator.clone(),
                    label:   "Blue".to_owned(),
                },
                "selectOption",
                serde_json::json!({"locator": locator, "label": "Blue"}),
            ),
            (
                StepCommand::Hover {
                    locator: locator.clone(),
                },
                "hover",
                serde_json::json!({"locator": locator}),
            ),
            (
                StepCommand::Upload {
                    locator: locator.clone(),
                    path:    "/abs/file.txt".to_owned(),
                },
                "upload",
                serde_json::json!({"locator": locator, "path": "/abs/file.txt"}),
            ),
            (
                StepCommand::Screenshot {
                    path: "/abs/shot.png".to_owned(),
                },
                "screenshot",
                serde_json::json!({"path": "/abs/shot.png"}),
            ),
            (
                StepCommand::Snapshot {
                    baseline_path: "/abs/base.png".to_owned(),
                    actual_path:   "/abs/actual.png".to_owned(),
                    diff_path:     "/abs/diff.png".to_owned(),
                    update:        false,
                },
                "snapshot",
                serde_json::json!({
                    "baselinePath": "/abs/base.png",
                    "actualPath": "/abs/actual.png",
                    "diffPath": "/abs/diff.png",
                    "update": false,
                }),
            ),
            (
                StepCommand::EvalAction {
                    script: "1 + 1".to_owned(),
                },
                "evalAction",
                serde_json::json!({"script": "1 + 1"}),
            ),
            (
                StepCommand::Page {
                    expect: serde_json::json!({"kind": "path", "value": "/dashboard"}),
                },
                "page",
                serde_json::json!({"expect": {"kind": "path", "value": "/dashboard"}}),
            ),
            (
                StepCommand::Assert {
                    spec: serde_json::json!({"subject": {"type": "url"}}),
                },
                "assert",
                serde_json::json!({"spec": {"subject": {"type": "url"}}}),
            ),
            (
                StepCommand::Capture {
                    source: serde_json::json!({"type": "title"}),
                    filter: Json::Null,
                },
                "capture",
                serde_json::json!({"source": {"type": "title"}, "filter": null}),
            ),
        ];
        for (command, expected_cmd, expected_params) in cases {
            let (cmd, params) = frame_of(&command);
            assert_eq!(cmd, expected_cmd);
            assert_eq!(params, expected_params, "params of {expected_cmd}");
        }
    }
}
