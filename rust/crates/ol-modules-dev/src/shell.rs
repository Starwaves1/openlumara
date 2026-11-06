//! Port of modules/shell/module.py: the AI's sandboxed shell. `run` executes inside a
//! persistent docker/podman container (or directly on the host when openlumara already runs
//! inside a container), with timeouts and output limits. Also the `shell`, `shell_setup` and
//! `shell_build` commands and the webui terminal routes.

#[path = "../../../generated/modules/shell.rs"]
pub mod meta;

use crate::pyos;
use anyhow::anyhow;
use async_trait::async_trait;
use md5::{Digest, Md5};
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{as_f64, as_i64, float_repr, py_str, py_str_opt, truthy};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// category used by the python module for its log lines
const LOG: &str = "sandbox_shell";
const FULL_IMAGE: &str = "openlumara_sandbox:latest";
const CONTAINER_NAME: &str = "openlumara_shell";
const HOME_DIR: &str = "/home/lumara";
const DOCKERFILE_MOUNT: &str = "/home/lumara/Dockerfile";
const PASSTHROUGH_SETUP: &str = "You're running openlumara in a container, so all commands are running directly in your container!";

/// python instance attributes that `on_ready` sets. `None` means the attribute does not
/// exist yet (python raises AttributeError); `runtime` and `container_name` hold an
/// inner Option because python stores None for them.
#[derive(Default)]
struct State {
    passthrough: bool,
    runtime: Option<Option<String>>,
    container_name: Option<Option<String>>,
    image_hash_path: Option<String>,
    host_workspace: Option<String>,
    host_user_uid: Option<u32>,
    host_user_gid: Option<u32>,
    use_gvisor: Option<bool>,
}

pub struct Shell {
    base: ModuleBase,
    state: Mutex<Option<State>>,
}

/// captured result of one host command (python `_run_async_cmd`)
#[derive(Debug, Clone, PartialEq)]
pub struct CmdOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub return_code: i64,
    pub timed_out: bool,
}

fn attr_error(name: &str) -> anyhow::Error {
    anyhow!("'Shell' object has no attribute '{}'", name)
}

fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `(body or {})` for a dict-like request body
fn body_dict(body: &Option<Value>) -> anyhow::Result<Map<String, Value>> {
    match body {
        None => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(v) if !truthy(v) => Ok(Map::new()),
        Some(v) => Err(anyhow!("'{}' object has no attribute 'get'", py_type_name(v))),
    }
}

/// python `bytes.__repr__`, used where the python code f-strings a bytes object
fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c == quote as u8 => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(c as char),
            c => out.push_str(&format!("\\x{:02x}", c)),
        }
    }
    out.push(quote);
    out
}

/// python `returncode` (negative signal number when killed), or -1 when unknown
fn exit_code_of(status: Option<ExitStatus>) -> i64 {
    match status {
        Some(s) => s.code().map(i64::from).or_else(|| s.signal().map(|sig| -i64::from(sig))).unwrap_or(-1),
        None => -1,
    }
}

/// `os.setsid` as the child's preexec_fn: new session, so the whole tree can be killed
fn set_session(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// `_is_running_in_container` cgroup check
pub fn cgroup_says_container(cgroup: &str) -> bool {
    ["docker", "podman", "libpod"].iter().any(|k| cgroup.contains(k))
}

fn is_running_in_container() -> anyhow::Result<bool> {
    if Path::new("/.dockerenv").exists() {
        return Ok(true);
    }
    if matches!(std::env::var("container").ok().as_deref(), Some("docker") | Some("podman")) {
        return Ok(true);
    }
    match std::fs::read_to_string("/proc/1/cgroup") {
        Ok(cgroup) => Ok(cgroup_says_container(&cgroup)),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => Ok(false),
        Err(e) => Err(anyhow!(pyos::os_err(&e, "/proc/1/cgroup"))),
    }
}

/// `_resolve_dockerfile_path`: expanded path, or the first existing candidate
fn resolve_dockerfile_path(dockerfile_path: &str) -> anyhow::Result<Option<String>> {
    if dockerfile_path.is_empty() {
        return Ok(None);
    }
    let expanded = pyos::expanduser(dockerfile_path);
    if Path::new(&expanded).is_absolute() {
        return Ok(Some(expanded));
    }
    // python: dirname(abspath(module.py)) == <project>/modules/shell
    let module_dir = pyos::join(&ol_core::paths::project_root().to_string_lossy(), "modules/shell");
    let candidates = [
        pyos::abspath(&expanded),
        pyos::abspath(&ol_core::paths::get_path(&expanded)?),
        pyos::abspath(&pyos::join(&pyos::join(&module_dir, ".."), &expanded)),
    ];
    for candidate in candidates {
        if pyos::isfile(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(Some(pyos::abspath(&expanded)))
}

/// `_parse_memory_string`: '10.23MiB' / '256m' -> bytes (kept as python wrote it, including
/// its quirks: '512MB' has its 'M' suffix sliced as one char and so yields 0)
pub fn parse_memory_string(mem_str: &str) -> f64 {
    let s = mem_str.trim().to_uppercase();
    if s.is_empty() {
        return 0.0;
    }
    let multipliers: [(&str, f64); 4] = [("K", 1024.0), ("M", 1024.0 * 1024.0), ("G", 1024.0 * 1024.0 * 1024.0), ("T", 1024.0 * 1024.0 * 1024.0 * 1024.0)];
    for (suffix, mult) in multipliers {
        if s.ends_with(&format!("{suffix}B")) || s.ends_with(suffix) {
            let cut = s.len() - suffix.len();
            return s[..cut].trim().parse::<f64>().map(|v| v * mult).unwrap_or(0.0);
        }
    }
    s.parse::<f64>().unwrap_or(0.0)
}

/// `_get_dockerfile_hash`: md5 hexdigest of the file
fn file_md5(path: &str) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).map_err(|e| anyhow!(pyos::os_err(&e, path)))?;
    let mut hasher = Md5::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// kill a process group (python `_kill_process_tree`); waits up to 2s for the child
async fn kill_process_tree(child: &mut Child) -> Option<ExitStatus> {
    let group_killed = match child.id() {
        Some(pid) => {
            let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
            pgid >= 0 && unsafe { libc::killpg(pgid, libc::SIGKILL) } == 0
        }
        None => false,
    };
    if !group_killed {
        let _ = child.start_kill();
    }
    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    }
}

/// python's `output_limit` value as the stream reader and the `len(...) >= output_limit`
/// checks see it
#[derive(Debug, Clone, PartialEq)]
pub struct OutputLimit {
    /// bytes a stream keeps (python `len(buffer) < limit`, then `chunk[:remaining]`)
    keep: i64,
    /// a positive float makes python's reader raise TypeError on its first chunk (float slice
    /// index) and stop reading; the error is swallowed by the reader's `except`
    stop_after_first_chunk: bool,
    /// the value `len(...) >= output_limit` compares against
    threshold: f64,
    /// python f-string interpolation of the raw value
    text: String,
}

impl OutputLimit {
    pub fn int(n: i64) -> Self {
        OutputLimit { keep: n.max(0), stop_after_first_chunk: false, threshold: n as f64, text: n.to_string() }
    }

    /// python's view of a config value; `Err(type name)` when `len(...) >= value` raises TypeError
    pub fn from_config(v: &Value) -> Result<Self, &'static str> {
        let limit = match v {
            Value::Bool(b) => Some(OutputLimit { text: if *b { "True" } else { "False" }.to_string(), ..Self::int(i64::from(*b)) }),
            Value::Number(n) if n.is_f64() => n.as_f64().map(|f| OutputLimit { keep: 0, stop_after_first_chunk: f > 0.0, threshold: f, text: float_repr(f) }),
            Value::Number(_) => as_i64(v).map(Self::int),
            _ => None,
        };
        limit.ok_or_else(|| py_type_name(v))
    }

    /// the reader used for a non-numeric limit: python raises on the first chunk, except for
    /// None, where it keeps everything. The output is discarded either way.
    fn incomparable(type_name: &str) -> Self {
        OutputLimit { keep: 0, stop_after_first_chunk: type_name != "NoneType", threshold: f64::NAN, text: String::new() }
    }
}

/// the text of python's TypeError from `len(stdout) >= output_limit` with a non-numeric limit
fn incomparable_limit_error(type_name: &str) -> String {
    format!("'>=' not supported between instances of 'int' and '{}'", type_name)
}

/// how long readers may keep draining once the child has exited (only matters when a background
/// grandchild still holds the pipes; a normal command reaches EOF at exit)
const DRAIN_GRACE: Duration = Duration::from_millis(100);

/// cancel a reader task and wait for it to stop; a task that already finished is not awaited
/// again (its output was taken or it is complete)
async fn stop_reader(task: &mut JoinHandle<()>) {
    if !task.is_finished() {
        task.abort();
        let _ = task.await;
    }
}

/// copy stream output into `buf` up to the limit, still draining the pipe past it
async fn read_stream<R: AsyncRead + Unpin>(mut stream: R, buf: Arc<Mutex<Vec<u8>>>, limit: Option<OutputLimit>) {
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut b = buf.lock();
                let take = match &limit {
                    None => n,
                    Some(l) => {
                        let cap = l.keep.max(0) as usize;
                        if b.len() < cap {
                            (cap - b.len()).min(n)
                        } else {
                            0
                        }
                    }
                };
                b.extend_from_slice(&chunk[..take]);
                if limit.as_ref().is_some_and(|l| l.stop_after_first_chunk) {
                    break;
                }
            }
        }
    }
}

/// python `_run_async_cmd`: run a host command in its own session with a timeout and an
/// optional output cap. Returns once the direct child exits (python `process.wait()`), not
/// at pipe EOF, so a background grandchild holding the pipes does not hold the result. On
/// timeout the process group is killed.
pub async fn run_cmd<S: AsRef<str>>(args: &[S], dur: Duration, limit: Option<OutputLimit>) -> anyhow::Result<CmdOutput> {
    let program = args.first().map(|a| a.as_ref().to_string()).ok_or_else(|| anyhow!("empty command"))?;
    let mut cmd = Command::new(&program);
    cmd.args(args.get(1..).unwrap_or(&[]).iter().map(|a| a.as_ref())).stdout(Stdio::piped()).stderr(Stdio::piped());
    set_session(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| anyhow!(pyos::os_err(&e, &program)))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(anyhow!("child pipes unavailable"));
    };

    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let mut out_task = tokio::spawn(read_stream(stdout, out_buf.clone(), limit.clone()));
    let mut err_task = tokio::spawn(read_stream(stderr, err_buf.clone(), limit));

    let (status, timed_out) = match timeout(dur, child.wait()).await {
        Ok(status) => {
            // keep whatever is already in the pipes, but never wait on a grandchild's EOF
            let _ = timeout(DRAIN_GRACE, async {
                let _ = (&mut out_task).await;
                let _ = (&mut err_task).await;
            })
            .await;
            stop_reader(&mut out_task).await;
            stop_reader(&mut err_task).await;
            (Some(status?), false)
        }
        Err(_elapsed) => {
            let status = kill_process_tree(&mut child).await;
            stop_reader(&mut out_task).await;
            stop_reader(&mut err_task).await;
            (status, true)
        }
    };

    let stdout = out_buf.lock().clone();
    let stderr = err_buf.lock().clone();
    Ok(CmdOutput { stdout, stderr, return_code: exit_code_of(status), timed_out })
}

/// the (stdout, stderr) text of a command, decoded like python `.decode('utf-8', errors='replace').strip()`
fn output_text(out: &CmdOutput) -> (String, String) {
    (String::from_utf8_lossy(&out.stdout).trim().to_string(), String::from_utf8_lossy(&out.stderr).trim().to_string())
}

fn output_limit_hit(out: &CmdOutput, limit: &OutputLimit) -> bool {
    out.stdout.len() as f64 >= limit.threshold || out.stderr.len() as f64 >= limit.threshold
}

/// result dict of a passthrough command: truncation counts as an error here
pub fn passthrough_results(out: &CmdOutput, timeout_val: &Value, limit: &OutputLimit) -> (Value, bool) {
    let (stdout, stderr) = output_text(out);
    let mut errors = Vec::new();
    if out.timed_out {
        errors.push(format!("Command execution timed out after {}s", py_str(timeout_val)));
    }
    if output_limit_hit(out, limit) {
        errors.push(format!("Output truncated - limit: {} chars", limit.text));
    }
    if out.return_code == 137 {
        errors.push("Process forcibly killed".to_string());
    }
    let mut results = json!({"stdout": stdout, "stderr": stderr, "exit_code": out.return_code});
    let success = errors.is_empty();
    if !success {
        results["errors"] = json!(errors);
    }
    (results, success)
}

/// result dict of a sandboxed command: truncation is informational (truncation_note), not a failure
pub fn sandbox_results(out: &CmdOutput, timeout_val: &Value, limit: &OutputLimit) -> (Value, bool) {
    let (stdout, stderr) = output_text(out);
    let mut results = json!({"stdout": stdout, "stderr": stderr, "exit_code": out.return_code});
    let mut errors = Vec::new();
    if out.timed_out {
        errors.push(format!("Command execution timed out after {}s", py_str(timeout_val)));
    }
    if output_limit_hit(out, limit) {
        results["truncation_note"] = json!(format!("Output truncated - limit: {} chars", limit.text));
    }
    if out.return_code == 137 {
        errors.push("Process forcibly killed".to_string());
    }
    let success = errors.is_empty();
    if !success {
        results["errors"] = json!(errors);
    }
    (results, success)
}

/// the text `cmd_shell` shows for a `run` result
pub fn format_shell_output(result: &Value) -> anyhow::Result<String> {
    let content = match result.get("content") {
        Some(c) if truthy(c) => c,
        _ => return Ok("error getting command output".to_string()),
    };
    let Value::Object(content) = content else {
        return Err(anyhow!("'{}' object has no attribute 'get'", py_type_name(content)));
    };
    let mut output = Vec::new();
    if let Some(stdout) = content.get("stdout").filter(|v| truthy(v)) {
        output.push(py_str(stdout));
    }
    if let Some(stderr) = content.get("stderr").filter(|v| truthy(v)) {
        output.push(py_str(stderr));
    }
    if let Some(note) = content.get("truncation_note").filter(|v| truthy(v)) {
        output.push(py_str(note));
    }
    if let Some(errors) = content.get("errors").filter(|v| truthy(v)) {
        let lines: Vec<String> = match errors {
            Value::Array(a) => a.iter().map(py_str).collect(),
            other => vec![py_str(other)],
        };
        output.push(format!("errors:\n{}", lines.join("\n")));
    }
    let joined = output.join("\n\n");
    Ok(if joined.is_empty() { "NO OUTPUT".to_string() } else { joined })
}

impl Shell {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, state: Mutex::new(None) }
    }

    // ---------------- state (python instance attributes)

    /// read an attribute set by on_ready; missing -> python AttributeError
    fn attr<T>(&self, name: &str, get: impl FnOnce(&State) -> Option<T>) -> anyhow::Result<T> {
        let g = self.state.lock();
        let s = g.as_ref().ok_or_else(|| attr_error("passthrough"))?;
        get(s).ok_or_else(|| attr_error(name))
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        if let Some(s) = self.state.lock().as_mut() {
            f(s);
        }
    }

    fn passthrough(&self) -> anyhow::Result<bool> {
        self.attr("passthrough", |s| Some(s.passthrough))
    }

    fn runtime(&self) -> anyhow::Result<Option<String>> {
        self.attr("runtime", |s| s.runtime.clone())
    }

    fn container_name(&self) -> anyhow::Result<Option<String>> {
        self.attr("container_name", |s| s.container_name.clone())
    }

    fn set_container_name(&self, v: Option<String>) {
        self.update(|s| s.container_name = Some(v));
    }

    fn image_hash_path(&self) -> anyhow::Result<String> {
        self.attr("image_hash_path", |s| s.image_hash_path.clone())
    }

    fn slog(&self, msg: &str) {
        self.base.log(LOG, msg);
    }

    /// config value with python's `config.get(key, default=...)` semantics
    fn cfg(&self, key: &str, default: Value) -> Value {
        self.base.config.get1(key).unwrap_or(default)
    }

    /// `self.config.get("run_as_user") or self.host_user_uid` (and the gid twin)
    fn owner_ids(&self) -> anyhow::Result<(String, String)> {
        let run_as = self.base.config.get1("run_as_user").filter(truthy);
        let uid = match &run_as {
            Some(v) => py_str(v),
            None => self.attr("host_user_uid", |s| s.host_user_uid)?.to_string(),
        };
        let gid = match &run_as {
            Some(v) => py_str(v),
            None => self.attr("host_user_gid", |s| s.host_user_gid)?.to_string(),
        };
        Ok((uid, gid))
    }

    /// `config.get("output_limit", default=2000)` as python sees it; `Err(type name)` when the
    /// stored value is not comparable with an int
    fn output_limit(&self) -> Result<OutputLimit, &'static str> {
        OutputLimit::from_config(&self.cfg("output_limit", json!(2000)))
    }

    /// `timeout_val + 5` as the safety timeout for a command
    fn safety_timeout(timeout_val: &Value) -> anyhow::Result<Duration> {
        let secs = as_f64(timeout_val).ok_or_else(|| anyhow!("unsupported operand type(s) for +: '{}' and 'int'", py_type_name(timeout_val)))?;
        Ok(Duration::from_secs_f64((secs + 5.0).clamp(0.0, 1.0e9)))
    }

    // ---------------- image build

    fn load_image_hash(&self) -> anyhow::Result<Option<String>> {
        let path = self.image_hash_path()?;
        Ok(std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string()))
    }

    /// `_save_image_hash`: IO errors are logged, not raised
    fn save_image_hash(&self, hash: &str) -> anyhow::Result<()> {
        let path = self.image_hash_path()?;
        let dir = pyos::dirname(&path);
        if let Err(e) = pyos::makedirs(&dir, true).and_then(|_| pyos::write_file(&path, hash.as_bytes())) {
            self.slog(&format!("Failed to save image hash: {}", e));
        }
        Ok(())
    }

    /// run `<runtime> build ...` with stderr merged into stdout, logging each non-empty line
    async fn run_build(&self, runtime: &str, uid: &str, use_buildx: bool, dockerfile_path: &str) -> anyhow::Result<(i64, Vec<String>)> {
        let (reader, writer) = std::io::pipe()?;
        let stdout_writer = writer.try_clone()?;
        let mut cmd = Command::new(runtime);
        // python passes the UID value as GID here too; kept as-is for fidelity
        let uid_arg = format!("UID={}", uid);
        let gid_arg = format!("GID={}", uid);
        cmd.args(["build", "--build-arg", uid_arg.as_str(), "--build-arg", gid_arg.as_str(), "-t", FULL_IMAGE, "-f", dockerfile_path, "."]);
        if use_buildx {
            cmd.env("DOCKER_BUILDKIT", "1");
        }
        cmd.stdout(Stdio::from(stdout_writer)).stderr(Stdio::from(writer));
        set_session(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| anyhow!(pyos::os_err(&e, runtime)))?;
        drop(cmd);

        let file = tokio::fs::File::from_std(std::fs::File::from(OwnedFd::from(reader)));
        let mut lines_reader = BufReader::new(file);
        let mut lines = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if lines_reader.read_until(b'\n', &mut buf).await? == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&buf).trim().to_string();
            if !text.is_empty() {
                self.slog(&format!("[build] {}", text));
                lines.push(text);
            }
        }
        let status = child.wait().await?;
        Ok((exit_code_of(Some(status)), lines))
    }

    /// `_build_image`: builds the configured Dockerfile when its hash changed (or when forced)
    async fn build_image(&self, force: bool) -> anyhow::Result<Option<String>> {
        if self.base.config.get1("method") != Some(json!("dockerfile")) {
            self.slog("Not using dockerfile method, skipping build.");
            return Ok(None);
        }
        let dockerfile_cfg = self.base.config.get1("dockerfile_path").unwrap_or(Value::Null);
        if !truthy(&dockerfile_cfg) {
            self.slog("No dockerfile_path configured.");
            return Ok(None);
        }
        let dockerfile_path = resolve_dockerfile_path(&py_str(&dockerfile_cfg))?.unwrap_or_default();
        if !pyos::isfile(&dockerfile_path) {
            self.slog(&format!("Dockerfile not found at {}, skipping build.", dockerfile_path));
            return Ok(None);
        }

        let current_hash = file_md5(&dockerfile_path)?;
        let stored_hash = self.load_image_hash()?;
        let unchanged = stored_hash.as_deref() == Some(current_hash.as_str());

        if !force && unchanged {
            // python wraps this whole check in try/except Exception: pass
            if let Ok(Some(runtime)) = self.runtime() {
                let args = [runtime.as_str(), "images", "--format", "{{.Repository}}:{{.Tag}}", FULL_IMAGE];
                if let Ok(out) = run_cmd(&args, Duration::from_secs(5), Some(OutputLimit::int(256))).await {
                    if std::str::from_utf8(&out.stdout).map(|s| s.contains(FULL_IMAGE)).unwrap_or(false) {
                        self.slog(&format!("Image {} is up to date (hash unchanged), skipping build.", FULL_IMAGE));
                        return Ok(Some(FULL_IMAGE.to_string()));
                    }
                }
            }
        }

        if force {
            self.slog(&format!("Force rebuild requested for {}", dockerfile_path));
        } else if !unchanged {
            self.slog(&format!("Dockerfile changed, rebuilding image {}", FULL_IMAGE));
        }

        let use_buildx = pyos::which("buildx").is_some();
        if use_buildx {
            self.slog("Using BuildKit (buildx) for image build.");
        } else {
            self.slog("buildx not found, using legacy builder.");
        }

        let (uid, _gid) = self.owner_ids()?;
        let runtime = self.runtime()?;
        let outcome = match runtime {
            Some(runtime) => self.run_build(&runtime, &uid, use_buildx, &dockerfile_path).await,
            None => Err(anyhow!("expected str, bytes or os.PathLike object, not NoneType")),
        };
        match outcome {
            Err(e) => {
                self.slog(&format!("Error building image: {}", e));
                Ok(None)
            }
            Ok((exit_code, lines)) => {
                if exit_code != 0 {
                    self.slog(&format!("Image build failed (exit code {})", exit_code));
                    for line in &lines[lines.len().saturating_sub(20)..] {
                        self.slog(&format!("[build] {}", line));
                    }
                    return Ok(None);
                }
                if let Err(e) = self.save_image_hash(&current_hash) {
                    self.slog(&format!("Error building image: {}", e));
                    return Ok(None);
                }
                self.slog(&format!("Image {} built successfully.", FULL_IMAGE));
                Ok(Some(FULL_IMAGE.to_string()))
            }
        }
    }

    // ---------------- container lifecycle

    /// `ps --filter name=<container>` contains the name; any failure counts as "no"
    async fn container_listed(&self, runtime: &str, name: &str) -> bool {
        let filter = format!("name={}", name);
        let args = [runtime, "ps", "--format", "{{.Names}}", "--filter", filter.as_str()];
        match run_cmd(&args, Duration::from_secs(5), Some(OutputLimit::int(256))).await {
            Ok(out) => std::str::from_utf8(&out.stdout).map(|s| s.contains(name)).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// `_start_container`: builds the image if needed and starts the persistent sandbox
    async fn start_container(&self) -> anyhow::Result<()> {
        let Some(runtime) = self.runtime()? else { return Ok(()) };

        let method = self.cfg("method", json!("dockerfile"));
        let method_is = |m: &str| method == json!(m);
        let dockerfile_cfg = self.base.config.get1("dockerfile_path").unwrap_or(Value::Null);
        let mut img = py_str(&self.cfg("image", json!("debian:stable-slim")));

        if method_is("dockerfile") {
            if truthy(&dockerfile_cfg) {
                match self.build_image(false).await? {
                    Some(built) => img = built,
                    None => self.slog("Build failed, using default image."),
                }
            } else {
                self.slog("No dockerfile_path set, using default image.");
            }
        } else if method_is("image") {
            self.slog(&format!("Using image method with: {}", img));
        } else {
            self.slog(&format!("Unknown method: {}, using default image.", py_str(&method)));
        }

        // python: `self.container_name` may be None here; the ps/rm steps then fail inside
        // their try/except and are skipped
        let container = self.container_name()?;
        if let Some(name) = &container {
            if self.container_listed(&runtime, name).await {
                self.slog(&format!("Container {} already running.", name));
                return Ok(());
            }
            let rm_args = [runtime.as_str(), "rm", "-f", name.as_str()];
            let _ = run_cmd(&rm_args, Duration::from_secs(10), None).await;
        }

        let (uid, gid) = self.owner_ids()?;
        let name = container.ok_or_else(|| anyhow!("sequence item 6: expected str instance, NoneType found"))?;

        let mut cmd: Vec<String> = vec![runtime.clone(), "run".into(), "-d".into(), "--rm".into(), "--init".into(), "--name".into(), name.clone()];
        if self.attr("use_gvisor", |s| s.use_gvisor)? {
            cmd.extend(["--runtime".to_string(), "runsc".to_string()]);
            if runtime == "podman" {
                cmd.extend(["--runtime-flag".to_string(), "ignore-cgroups".to_string()]);
            }
        }
        let internet = truthy(&self.cfg("internet_access", json!(false)));
        cmd.extend([
            "--user".to_string(),
            format!("{}:{}", uid, gid),
            "--cap-drop".into(),
            "ALL".into(),
            "--cap-add".into(),
            "KILL".into(),
            "--read-only".into(),
            "--security-opt".into(),
            "no-new-privileges:true".into(),
            "--cpus".into(),
            py_str(&self.cfg("cpu_limit", json!(0.5))),
            "--memory".into(),
            py_str(&self.cfg("memory_limit", json!("512m"))),
            "--pids-limit".into(),
            py_str(&self.cfg("max_processes", json!(10))),
            "--network".into(),
            (if internet { "bridge" } else { "none" }).into(),
            "--stop-timeout".into(),
            "1".into(),
        ]);

        if truthy(&self.cfg("persistent_data", json!(true))) {
            let workspace = self.attr("host_workspace", |s| s.host_workspace.clone())?;
            cmd.extend(["-v".to_string(), format!("{}:{}:Z", workspace, HOME_DIR)]);
        } else {
            let limit = py_str(&self.cfg("temporary_filesystem_size_limit", json!("512m")));
            cmd.extend(["--tmpfs".to_string(), format!("{}:size={}", HOME_DIR, limit)]);
        }

        if method_is("dockerfile") && truthy(&dockerfile_cfg) {
            let expanded = resolve_dockerfile_path(&py_str(&dockerfile_cfg))?.unwrap_or_default();
            if pyos::isfile(&expanded) {
                cmd.extend(["-v".to_string(), format!("{}:{}:ro", expanded, DOCKERFILE_MOUNT)]);
                self.slog(&format!("Mounted Dockerfile read-only at {}", DOCKERFILE_MOUNT));
            }
        }

        cmd.extend(["-w".to_string(), HOME_DIR.to_string(), img.clone()]);

        self.base.log(&self.base.name, &format!("starting using command: {}", cmd.join(" ")));

        match run_cmd(&cmd, Duration::from_secs(30), Some(OutputLimit::int(1024 * 1024))).await {
            Ok(out) => {
                if !out.stderr.is_empty() {
                    self.slog(&format!("ERROR: {}", py_bytes_repr(&out.stderr)));
                    self.set_container_name(None);
                    return Ok(());
                }
                self.slog(&format!("Container {} started (UID: {}, Image: {}, method: {}).", name, uid, img, py_str(&method)));
            }
            Err(e) => {
                self.slog(&format!("Error starting container: {}", e));
                self.set_container_name(None);
            }
        }
        Ok(())
    }

    /// `_is_container_running`
    async fn is_container_running(&self) -> anyhow::Result<bool> {
        let Some(runtime) = self.runtime()? else { return Ok(false) };
        let Some(name) = self.container_name()? else { return Ok(false) };
        Ok(self.container_listed(&runtime, &name).await)
    }

    /// `_stop_container`: stop and remove, errors ignored
    async fn stop_container(&self) {
        let (Ok(Some(name)), Ok(Some(runtime))) = (self.container_name(), self.runtime()) else { return };
        let stop_args = [runtime.as_str(), "stop", name.as_str()];
        let _ = run_cmd(&stop_args, Duration::from_secs(10), None).await;
        let rm_args = [runtime.as_str(), "rm", "-f", name.as_str()];
        let _ = run_cmd(&rm_args, Duration::from_secs(10), None).await;
    }

    // ---------------- running commands

    /// `_run_passthrough`: run directly on the host when openlumara is already in a container
    async fn run_passthrough(&self, command: &str) -> anyhow::Result<Value> {
        let timeout_val = self.cfg("execution_timeout", json!(10));
        let limit = self.output_limit();
        let safety = Self::safety_timeout(&timeout_val)?;
        let reader = match &limit {
            Ok(l) => l.clone(),
            Err(t) => OutputLimit::incomparable(t),
        };
        match run_cmd(&["sh", "-c", command], safety, Some(reader)).await {
            Ok(out) => match &limit {
                Ok(limit) => {
                    let (results, success) = passthrough_results(&out, &timeout_val, limit);
                    Ok(self.base.result(results, success))
                }
                Err(t) => Ok(self.base.result(format!("Error while running passthrough shell command: {}", incomparable_limit_error(t)), false)),
            },
            Err(e) => Ok(self.base.result(format!("Error while running passthrough shell command: {}", e), false)),
        }
    }

    /// `run`: execute a command in the persistent sandbox container
    pub async fn run(&self, command: &str) -> anyhow::Result<Value> {
        if self.passthrough()? {
            return self.run_passthrough(command).await;
        }
        let Some(runtime) = self.runtime()? else {
            return Ok(self.base.result("Docker or podman not available.", false));
        };
        if self.container_name()?.is_none() {
            return Ok(self.base.result("Sandbox container not initialized.", false));
        }

        // the container can die at any time (e.g. OOM); with --rm it's gone, so start a fresh one
        if !self.is_container_running().await? {
            self.slog("Container is not running, restarting it.");
            self.start_container().await?;
            if !self.is_container_running().await? {
                return Ok(self.base.result("Sandbox container crashed and could not be restarted.", false));
            }
        }
        let container = self.container_name()?.ok_or_else(|| anyhow!("argument must be str, not None"))?;

        let timeout_val = self.cfg("execution_timeout", json!(10));
        let limit = self.output_limit();
        let safety = Self::safety_timeout(&timeout_val)?;
        let timeout_str = py_str(&timeout_val);
        let args = [
            runtime.as_str(),
            "exec",
            container.as_str(),
            "timeout",
            "-k",
            "1",
            "-s",
            "KILL",
            timeout_str.as_str(),
            "sh",
            "-c",
            command,
        ];
        let reader = match &limit {
            Ok(l) => l.clone(),
            Err(t) => OutputLimit::incomparable(t),
        };
        match run_cmd(&args, safety, Some(reader)).await {
            Ok(out) => match &limit {
                Ok(limit) => {
                    let (results, success) = sandbox_results(&out, &timeout_val, limit);
                    Ok(self.base.result(results, success))
                }
                Err(t) => Ok(self.base.result(format!("Error while running sandboxed shell command: {}", incomparable_limit_error(t)), false)),
            },
            Err(e) => Ok(self.base.result(format!("Error while running sandboxed shell command: {}", e), false)),
        }
    }

    /// `cmd_shell`
    async fn cmd_shell(&self, args: &[String]) -> anyhow::Result<String> {
        if args.is_empty() {
            return Ok("Usage: shell <command>".to_string());
        }
        let command = args.join(" ");
        // run() always returns a result dict, so python's non-dict fallback is unreachable
        let result = self.run(&command).await?;
        format_shell_output(&result)
    }

    /// `_get_setup`: human-readable sandbox summary (system prompt, `shell_setup`, webui banner)
    fn get_setup(&self) -> anyhow::Result<String> {
        if self.passthrough()? {
            return Ok(PASSTHROUGH_SETUP.to_string());
        }
        if self.base.config.get1("run_as_user").filter(truthy).is_none() {
            self.attr("host_user_gid", |s| s.host_user_gid)?;
        }
        let method = py_str(&self.cfg("method", json!("dockerfile")));
        let mut lines = vec![
            format!("Runtime: {}", self.runtime()?.filter(|r| !r.is_empty()).unwrap_or_else(|| "Not available".into())),
            format!("Container Name: {}", self.container_name()?.filter(|c| !c.is_empty()).unwrap_or_else(|| "Not running".into())),
            format!("method: {}", method),
            format!(
                "Internet Access: {}",
                if truthy(&self.base.config.get1("internet_access").unwrap_or(Value::Null)) { "enabled" } else { "disabled" }
            ),
            format!("Persistent Data: {}", py_str(&self.cfg("persistent_data", json!(true)))),
            format!("gVisor Enabled: {}", if self.attr("use_gvisor", |s| s.use_gvisor)? { "True" } else { "False" }),
        ];
        if method == "dockerfile" {
            match self.base.config.get1("dockerfile_path").filter(truthy) {
                Some(dp) => {
                    let dp = py_str(&dp);
                    let expanded = resolve_dockerfile_path(&dp)?.unwrap_or_default();
                    let exists = if pyos::isfile(&expanded) { "exists" } else { "NOT FOUND" };
                    lines.push(format!("Dockerfile: {} ({})", dp, exists));
                }
                None => lines.push("Dockerfile: (not set)".to_string()),
            }
        } else if method == "image" {
            lines.push(format!("Image: {}", py_str_opt(self.base.config.get1("image").as_ref())));
        }
        Ok(lines.join("\n"))
    }

    /// `cmd_build`: rebuild the sandbox image and restart the container with it
    async fn cmd_build(&self) -> anyhow::Result<String> {
        if self.runtime()?.is_none() {
            return Ok("Docker or podman not available.".to_string());
        }
        if self.cfg("method", json!("dockerfile")) != json!("dockerfile") {
            return Ok("Not using dockerfile method. Switch to 'dockerfile' in module config to use this command.".to_string());
        }
        if !truthy(&self.base.config.get1("dockerfile_path").unwrap_or(Value::Null)) {
            return Ok("No Dockerfile configured. Set 'dockerfile_path' in module config.".to_string());
        }
        if self.build_image(true).await?.is_some() {
            self.stop_container().await;
            self.start_container().await?;
            Ok("Image rebuilt successfully. Container restarted with new image.".to_string())
        } else {
            Ok("Image build failed. Check logs for details.".to_string())
        }
    }

    /// `_route_run`: the webui terminal runs through the same run() path as the AI
    async fn route_run(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let obj = body_dict(body)?;
        let empty = Value::String(String::new());
        let command = py_str(obj.get("command").unwrap_or(&empty)).trim().to_string();
        if command.is_empty() {
            return Ok(self.base.result("no command given", false));
        }
        self.run(&command).await
    }
}

#[async_trait]
impl Module for Shell {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn header(&self) -> Option<String> {
        meta::HEADER.map(|h| h.to_string())
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "run" => self.run(&args.str_or("command", "")).await,
            _ => anyhow::bail!("'Shell' object has no attribute '{}'", method),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        let passthrough = is_running_in_container()?;
        if passthrough {
            *self.state.lock() = Some(State { passthrough: true, ..State::default() });
            self.slog("Detected we are already running inside a container. Running commands directly on the host!");
            return Ok(());
        }

        // hash tracking file location
        let image_hash_path = ol_core::paths::get_data_path(Some("shell/docker_image_hash"))?;
        *self.state.lock() = Some(State {
            passthrough: false,
            runtime: Some(None),
            container_name: Some(Some(CONTAINER_NAME.to_string())),
            image_hash_path: Some(image_hash_path),
            ..State::default()
        });

        let runtime = if pyos::which("podman").is_some() {
            Some("podman".to_string())
        } else if pyos::which("docker").is_some() {
            Some("docker".to_string())
        } else {
            None
        };
        let available = runtime.is_some();
        self.update(|s| s.runtime = Some(runtime));
        if !available {
            self.slog("Neither docker nor podman are available!");
            return Ok(());
        }

        let sandbox_path = py_str(&self.cfg("sandbox_path", json!("~/sandbox")));
        let host_workspace = ol_core::paths::get_path(&pyos::expanduser(&sandbox_path))?;
        pyos::makedirs(&host_workspace, true).map_err(|e| anyhow!(e))?;
        let host_user_uid = unsafe { libc::getuid() };
        let host_user_gid = unsafe { libc::getgid() };
        let workspace = host_workspace.clone();
        self.update(|s| {
            s.host_workspace = Some(workspace);
            s.host_user_uid = Some(host_user_uid);
            s.host_user_gid = Some(host_user_gid);
        });

        let use_gvisor = pyos::which("runsc").is_some();
        self.update(|s| s.use_gvisor = Some(use_gvisor));
        if use_gvisor {
            self.slog("gVisor (runsc) detected. Sandbox will use gVisor for enhanced security.");
        } else {
            self.slog("Warning: gVisor (runsc) not found. Sandbox is running with standard isolation.");
        }

        self.start_container().await
    }

    async fn on_shutdown(&self) -> anyhow::Result<()> {
        if self.passthrough()? {
            return Ok(());
        }
        if let (Some(name), Some(_runtime)) = (self.container_name()?, self.runtime()?) {
            self.slog(&format!("Shutting down container {}...", name));
            self.stop_container().await;
            self.set_container_name(None);
            self.slog("Container removed.");
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        Ok(Some(self.get_setup()?))
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "shell" => Ok(Some(self.cmd_shell(&args).await?)),
            "shell_setup" => Ok(Some(self.get_setup()?)),
            "shell_build" => Ok(Some(self.cmd_build().await?)),
            _ => anyhow::bail!("unknown command {}", name),
        }
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn ui_route(&self, path: &str, _method: &str, body: Option<Value>, _query: Map<String, Value>) -> anyhow::Result<Value> {
        match path {
            "run" => self.route_run(&body).await,
            "status" => Ok(json!(match self.get_setup() {
                Ok(text) => text,
                Err(e) => format!("sandbox not initialized: {}", ol_core::detail_error(&e)),
            })),
            _ => anyhow::bail!("route not found: {}", path),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn out(stdout: &[u8], stderr: &[u8], code: i64, timed_out: bool) -> CmdOutput {
        CmdOutput { stdout: stdout.to_vec(), stderr: stderr.to_vec(), return_code: code, timed_out }
    }

    #[test]
    fn memory_strings_parse_like_python() {
        assert_eq!(parse_memory_string("512m"), 512.0 * 1024.0 * 1024.0);
        assert_eq!(parse_memory_string(" 2G "), 2.0 * 1024.0 * 1024.0 * 1024.0);
        assert_eq!(parse_memory_string("1.5k"), 1536.0);
        assert_eq!(parse_memory_string("300"), 300.0);
        assert_eq!(parse_memory_string(""), 0.0);
        assert_eq!(parse_memory_string("10MIB"), 0.0);
        // python slices one char for 'MB', leaving '10M', which float() rejects
        assert_eq!(parse_memory_string("10MB"), 0.0);
    }

    #[test]
    fn container_detection_from_cgroup() {
        assert!(cgroup_says_container("12:memory:/docker/abc123"));
        assert!(cgroup_says_container("0::/libpod-payload-xyz"));
        assert!(!cgroup_says_container("0::/init.scope"));
    }

    #[test]
    fn dockerfile_hash_is_md5_hexdigest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Dockerfile");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(file_md5(&p.to_string_lossy()).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn bytes_repr_matches_python() {
        assert_eq!(py_bytes_repr(b"a'b\n"), "b\"a'b\\n\"");
        assert_eq!(py_bytes_repr(b"x\x01"), "b'x\\x01'");
    }

    #[test]
    fn passthrough_truncation_is_an_error_and_timeout_message() {
        let o = out(b"hel", b"", 0, false);
        let (results, success) = passthrough_results(&o, &json!(10), &OutputLimit::int(3));
        assert!(!success);
        assert_eq!(results, json!({"stdout": "hel", "stderr": "", "exit_code": 0, "errors": ["Output truncated - limit: 3 chars"]}));

        let killed = out(b"", b"", 137, true);
        let (results, success) = passthrough_results(&killed, &json!(10), &OutputLimit::int(2000));
        assert!(!success);
        assert_eq!(
            results["errors"],
            json!(["Command execution timed out after 10s", "Process forcibly killed"])
        );
    }

    #[test]
    fn sandbox_truncation_is_informational() {
        let o = out(b"hel", b"", 0, false);
        let (results, success) = sandbox_results(&o, &json!(30), &OutputLimit::int(3));
        assert!(success);
        assert_eq!(results, json!({"stdout": "hel", "stderr": "", "exit_code": 0, "truncation_note": "Output truncated - limit: 3 chars"}));
    }

    #[test]
    fn shell_output_formatting() {
        let r = json!({"status": "success", "content": {"stdout": "out", "stderr": "err", "exit_code": 0, "truncation_note": "Output truncated - limit: 3 chars", "errors": ["timed out"]}});
        assert_eq!(format_shell_output(&r).unwrap(), "out\n\nerr\n\nOutput truncated - limit: 3 chars\n\nerrors:\ntimed out");
        let empty = json!({"status": "success", "content": {"stdout": "", "stderr": "", "exit_code": 0}});
        assert_eq!(format_shell_output(&empty).unwrap(), "NO OUTPUT");
        let no_content = json!({"status": "error", "content": ""});
        assert_eq!(format_shell_output(&no_content).unwrap(), "error getting command output");
        let text_content = json!({"status": "error", "content": "Docker or podman not available."});
        assert_eq!(format_shell_output(&text_content).unwrap_err().to_string(), "'str' object has no attribute 'get'");
    }

    #[test]
    fn run_cmd_captures_stdout_and_exit_code() {
        let o = rt().block_on(run_cmd(&["sh", "-c", "echo hi; exit 3"], Duration::from_secs(10), None)).unwrap();
        assert_eq!(o.stdout, b"hi\n");
        assert_eq!(o.return_code, 3);
        assert!(!o.timed_out);
    }

    #[test]
    fn run_cmd_kills_process_group_on_timeout() {
        let o = rt().block_on(run_cmd(&["sh", "-c", "sleep 30"], Duration::from_millis(300), Some(OutputLimit::int(100)))).unwrap();
        assert!(o.timed_out);
        assert_eq!(o.return_code, -(libc::SIGKILL as i64));
    }

    #[test]
    fn passthrough_run_returns_python_result_dict() {
        let shell = Shell::new(ModuleBase::detached("shell"));
        *shell.state.lock() = Some(State { passthrough: true, ..State::default() });
        let result = rt().block_on(shell.run("echo hi")).unwrap();
        assert_eq!(result, json!({"status": "success", "content": {"stdout": "hi", "stderr": "", "exit_code": 0}}));
    }

    #[test]
    fn setup_text_for_passthrough_and_uninitialised_state() {
        let shell = Shell::new(ModuleBase::detached("shell"));
        let err = shell.get_setup().unwrap_err();
        assert_eq!(err.to_string(), "'Shell' object has no attribute 'passthrough'");
        *shell.state.lock() = Some(State { passthrough: true, ..State::default() });
        assert_eq!(shell.get_setup().unwrap(), PASSTHROUGH_SETUP);
    }

    #[test]
    fn run_cmd_returns_when_child_exits_with_background_grandchild() {
        // the `sleep` inherits the pipes; python's process.wait() returns as soon as `sh` exits
        let started = std::time::Instant::now();
        let o = rt().block_on(run_cmd(&["sh", "-c", "sleep 30 & echo \"$!\"; echo hi"], Duration::from_secs(10), Some(OutputLimit::int(2000)))).unwrap();
        assert!(!o.timed_out);
        assert_eq!(o.return_code, 0);
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        let mut lines = text.lines();
        let grandchild: libc::pid_t = lines.next().unwrap().parse().unwrap();
        assert_eq!(lines.next(), Some("hi"));
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn float_output_limit_keeps_no_output_like_python() {
        // python's reader slices with a float, raises, and the error is swallowed: nothing is kept
        let limit = OutputLimit::from_config(&json!(2000.0)).unwrap();
        let o = rt().block_on(run_cmd(&["sh", "-c", "echo hi"], Duration::from_secs(10), Some(limit.clone()))).unwrap();
        assert_eq!(o.stdout, b"");
        assert_eq!(o.return_code, 0);
        let (results, success) = passthrough_results(&o, &json!(10), &limit);
        assert!(success);
        assert_eq!(results, json!({"stdout": "", "stderr": "", "exit_code": 0}));
    }

    #[test]
    fn non_int_output_limits_render_like_python() {
        // a float limit is interpolated as python prints it
        let zero = OutputLimit::from_config(&json!(0.0)).unwrap();
        let (results, success) = sandbox_results(&out(b"", b"", 0, false), &json!(10), &zero);
        assert!(success);
        assert_eq!(results, json!({"stdout": "", "stderr": "", "exit_code": 0, "truncation_note": "Output truncated - limit: 0.0 chars"}));
        // a bool is an int in python (True == 1) and prints as True
        let no = OutputLimit::from_config(&json!(false)).unwrap();
        let (results, _) = passthrough_results(&out(b"", b"", 0, false), &json!(10), &no);
        assert_eq!(results["errors"], json!(["Output truncated - limit: False chars"]));
        // a numeric string is not comparable with len(): python raises TypeError
        assert_eq!(OutputLimit::from_config(&json!("2000")).unwrap_err(), "str");
        assert_eq!(incomparable_limit_error("str"), "'>=' not supported between instances of 'int' and 'str'");
    }
}
