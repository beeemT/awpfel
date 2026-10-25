//! Owned Apfel sidecar lifecycle.
//!
//! The awpfel daemon needs an OpenAI-compatible backend on the dedicated
//! loopback endpoint `http://127.0.0.1:11435/v1` serving
//! `apple-foundationmodel`. `awpfel start` ensures such a backend is healthy:
//!
//! - An already-healthy endpoint is reused untouched (never killed), whether it
//!   is a previous owned sidecar or an externally started Apfel.
//! - Otherwise an owned `apfel --serve` child is spawned on the dedicated
//!   loopback port, waited on with a bounded deadline, and reaped when the
//!   daemon shuts down. Interruption (SIGTERM/SIGINT) during the wait kills
//!   and reaps the partial child so no untracked server is left behind.
//!
//! Readiness means the `/v1/models` identity check *and* `/health` reporting
//! `model_available: true`; models-listing alone would let an unavailable
//! Apple Intelligence falsely pass startup.
//!
//! Only processes spawned by this process (via the `Child` handle, which pins
//! the pid against reuse) or re-verified by exact executable/argv plus saved
//! process-start identity are ever signaled. Anything unverifiable is left
//! alone. Global `apfel` on port 11434, `brew services` state, and Ollama are
//! never touched.

use awpfel::config;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const APFEL_HOST: &str = "127.0.0.1";
pub const APFEL_PORT: u16 = 11435;
pub const APFEL_MODEL: &str = "apple-foundationmodel";

/// How long to wait for a freshly spawned (or still-loading) Apfel to serve
/// the expected model before giving up with an actionable error.
const READINESS_DEADLINE: Duration = Duration::from_secs(60);
const READINESS_POLL: Duration = Duration::from_millis(500);
/// Per-request timeout for backend probes; keeps `start`/`status` snappy when
/// nothing is listening.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for an owned sidecar to exit after SIGTERM before SIGKILL.
const STOP_DEADLINE: Duration = Duration::from_secs(5);

pub fn default_base_url() -> String {
    format!("http://{APFEL_HOST}:{APFEL_PORT}/v1")
}

/// Whether `base_url` points at the dedicated endpoint this module manages.
/// Parsed as a URL: the host must be loopback, the port exactly 11435, and
/// the path the `/v1` API root. Substring lookalikes
/// (`http://127.0.0.1:11435.evil.example/v1`,
/// `http://evil.example/?x=127.0.0.1:11435`) never match. A custom
/// `ai.base_url` is only health-checked, never auto-spawned onto.
pub fn is_managed_endpoint(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url.trim_end_matches('/')) else {
        return false;
    };
    let host_loopback = matches!(
        url.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("::1")
    );
    let port_managed = url.port() == Some(APFEL_PORT);
    let path_api = url.path() == "/v1" || url.path() == "/v1/";
    host_loopback && port_managed && path_api
}

/// Shared interruption flag, set once by the startup signal handlers and
/// polled/awaited across backend ensure and daemon startup. Stays set: every
/// waiter observes the cancellation.
#[derive(Debug, Default)]
pub struct CancelFlag {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        // Register the waiter BEFORE checking the flag: a cancel() landing
        // between the check and the registration would otherwise lose its
        // wakeup and sleep forever.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        while !self.is_cancelled() {
            notified.as_mut().await;
            notified.set(self.notify.notified());
        }
    }
}

pub enum EnsureOutcome {
    HealthyExisting,
    StartedOwned(OwnedApfel),
    Unavailable(String),
    Cancelled,
}

/// An Apfel server spawned by this process. The `Child` handle pins the pid
/// against reuse until reaped, so signaling it is always safe.
pub struct OwnedApfel {
    child: tokio::process::Child,
}

impl OwnedApfel {
    /// Stop the owned sidecar gracefully (SIGTERM, then SIGKILL on timeout)
    /// and reap it. Safe by construction: the pid belongs to our live child.
    pub async fn stop(mut self) {
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let start = Instant::now();
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if start.elapsed() < STOP_DEADLINE => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    _ => {
                        self.child.kill().await.ok();
                        let _ = self.child.wait().await;
                        break;
                    }
                }
            }
        } else {
            let _ = self.child.wait().await;
        }
        remove_sidecar_pid();
    }
}

/// Ensure a healthy Apfel backend for `base_url`/`model`.
///
/// Never kills or restarts an already-healthy endpoint. Spawns an owned
/// `apfel --serve` on the dedicated loopback port only when nothing usable is
/// listening and the configured endpoint is the managed one. `api_key`, when
/// configured, is sent as a Bearer token on probes (custom authenticated
/// endpoints). `cancel` aborts a pending readiness wait, killing and reaping
/// any partial child first.
pub async fn ensure_local_apfel(
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    cancel: &CancelFlag,
) -> EnsureOutcome {
    if cancel.is_cancelled() {
        return EnsureOutcome::Cancelled;
    }
    let base = base_url.trim_end_matches('/').to_string();
    let managed = is_managed_endpoint(&base);
    let client = probe_client();

    if check_backend_with(&client, &base, model, api_key, managed).await {
        // Healthy: a stale record (dead pid) is cleaned; anything else is
        // left strictly alone — reuse, never adopt-and-kill.
        match read_sidecar_record() {
            Some(record) if record.pid != 0 && pid_alive(record.pid) => {}
            Some(_) => remove_sidecar_pid(),
            None => {}
        }
        tracing::info!("using existing healthy apfel backend at {base}");
        return EnsureOutcome::HealthyExisting;
    }

    if !managed {
        return EnsureOutcome::Unavailable(format!(
            "configured ai.base_url {base} is not reachable, and awpfel only manages \
             its default endpoint {}. Point ai.base_url at a running server or reset \
             it to the default and run `awpfel start` again.",
            default_base_url()
        ));
    }

    // Nothing usable: a previous owned sidecar may still be loading the model.
    // Wait for it, but never signal it: only this process's own children are
    // ever killed.
    if let Some(record) = read_sidecar_record()
        && record.pid != 0
        && verify_record(&record)
    {
        tracing::info!(
            "waiting for previously started apfel sidecar (pid {}) to become ready",
            record.pid
        );
        if wait_ready(&client, &base, model, api_key, READINESS_DEADLINE, cancel).await {
            return EnsureOutcome::HealthyExisting;
        }
        if cancel.is_cancelled() {
            return EnsureOutcome::Cancelled;
        }
        return EnsureOutcome::Unavailable(not_ready_message(true));
    }
    // Stale record (dead or unverified pid): clean up, never kill.
    remove_sidecar_pid();

    if port_answers(&client, &base).await {
        return EnsureOutcome::Unavailable(format!(
            "port {APFEL_PORT} already serves a backend that is not Apfel \
             (expected model `{model}`). Free the port or stop that server, then run \
             `awpfel start` again. Nothing was stopped automatically."
        ));
    }

    if cancel.is_cancelled() {
        return EnsureOutcome::Cancelled;
    }
    let mut child = match spawn_sidecar() {
        Ok(child) => child,
        Err(e) => {
            return EnsureOutcome::Unavailable(format!(
                "could not start Apfel backend ({e}). Install it with `brew install apfel` \
                 (Apple Silicon, macOS 26+), then run `awpfel start` again."
            ));
        }
    };
    let Some(pid) = child.id() else {
        let _ = child.wait().await;
        return EnsureOutcome::Unavailable(
            "the spawned `apfel --serve` exited immediately; see the log for details.".to_string(),
        );
    };
    tracing::info!("spawned owned apfel sidecar (pid {pid}) on {APFEL_HOST}:{APFEL_PORT}");

    // Persist ownership BEFORE the readiness wait so an interruption (even
    // SIGKILL of this process) leaves a verifiable record instead of an
    // untracked server. The record pins pid + process-start identity + argv0.
    let written = match SidecarRecord::capture(pid) {
        Some(record) => {
            let written = record.serialize();
            write_sidecar_record(&written);
            written
        }
        None => {
            // Unverifiable: do not leave an untracked server behind. Our own
            // child handle makes kill/reap safe.
            child.kill().await.ok();
            let _ = child.wait().await;
            return EnsureOutcome::Unavailable(
                "could not verify the spawned apfel process identity; stopped it rather \
                 than leave an untracked server behind."
                    .to_string(),
            );
        }
    };

    // Bounded wait; also notice if the child exits early (e.g. port taken by a
    // non-HTTP process, Apple Intelligence unavailable at exec time).
    let start = Instant::now();
    let ready = loop {
        if check_backend_with(&client, &base, model, api_key, true).await {
            break true;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!("owned apfel sidecar exited early: {status}");
                break false;
            }
            Ok(None) => {}
            Err(_) => break false,
        }
        if start.elapsed() >= READINESS_DEADLINE {
            break check_backend_with(&client, &base, model, api_key, true).await;
        }
        tokio::select! {
            _ = tokio::time::sleep(READINESS_POLL) => {}
            _ = cancel.cancelled() => {
                child.kill().await.ok();
                let _ = child.wait().await;
                remove_sidecar_record_if(&written);
                return EnsureOutcome::Cancelled;
            }
        }
    };

    if ready {
        return EnsureOutcome::StartedOwned(OwnedApfel { child });
    }

    child.kill().await.ok();
    let _ = child.wait().await;
    remove_sidecar_record_if(&written);
    EnsureOutcome::Unavailable(not_ready_message(false))
}

/// Probe the backend: `/v1/models` must list `model`, and — when
/// `require_health` — `/health` must report the model available. Custom
/// endpoints skip the health gate; the managed endpoint always requires it.
pub async fn check_backend(
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    require_health: bool,
) -> bool {
    check_backend_with(&probe_client(), base_url, model, api_key, require_health).await
}

fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
) -> Option<serde_json::Value> {
    let mut request = client.get(url);
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

async fn check_backend_with(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    require_health: bool,
) -> bool {
    let base = base_url.trim_end_matches('/');
    let models_url = format!("{base}/models");
    let models = match get_json(client, &models_url, api_key).await {
        Some(models) => models,
        None => return false,
    };
    if !models_lists_model(&models, model) {
        return false;
    }
    if !require_health {
        return true;
    }
    let Some(health_url) = health_url(base) else {
        return false;
    };
    match get_json(client, health_url.as_str(), api_key).await {
        Some(health) => health_is_available(&health, model),
        None => false,
    }
}

fn health_url(base_url: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url.trim_end_matches('/')).ok()?;
    url.set_path("/health");
    url.set_query(None);
    Some(url)
}

/// Whether the `/v1/models` document lists `model`. This identity check is
/// what keeps us from mistaking an unrelated server (Ollama, vLLM, a proxy)
/// for Apfel. `check_backend` parses the response body directly; this
/// raw-document entry point exists so behavioral tests pin the wire shape.
#[cfg(test)]
pub fn backend_serves_model(models_body: &str, model: &str) -> bool {
    let value: serde_json::Value = match serde_json::from_str(models_body) {
        Ok(value) => value,
        Err(_) => return false,
    };
    models_lists_model(&value, model)
}

fn models_lists_model(models: &serde_json::Value, model: &str) -> bool {
    match models.get("data").and_then(|data| data.as_array()) {
        Some(models) => models
            .iter()
            .any(|entry| entry.get("id").and_then(|id| id.as_str()) == Some(model)),
        None => false,
    }
}

/// Whether `/health` reports the model actually available. A listing in
/// `/v1/models` alone is not readiness: unavailable Apple Intelligence must
/// not falsely pass startup. Raw-document entry point so behavioral tests pin
/// the `/health` wire shape.
#[cfg(test)]
pub fn health_reports_available(health_body: &str, model: &str) -> bool {
    let value: serde_json::Value = match serde_json::from_str(health_body) {
        Ok(value) => value,
        Err(_) => return false,
    };
    health_is_available(&value, model)
}

fn health_is_available(health: &serde_json::Value, model: &str) -> bool {
    if health.get("model_available").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }
    match health.get("model").and_then(|v| v.as_str()) {
        Some(serving) => serving == model,
        None => true,
    }
}

/// Whether something answers HTTP on the endpoint at all (regardless of
/// identity). Used to report "occupied by a foreign server" instead of
/// spawning into a port conflict.
async fn port_answers(client: &reqwest::Client, base_url: &str) -> bool {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    client.get(&url).send().await.is_ok()
}

async fn wait_ready(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    deadline: Duration,
    cancel: &CancelFlag,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cancel.is_cancelled() {
            return false;
        }
        if check_backend_with(client, base_url, model, api_key, true).await {
            return true;
        }
        tokio::select! {
            _ = tokio::time::sleep(READINESS_POLL) => {}
            _ = cancel.cancelled() => return false,
        }
    }
    !cancel.is_cancelled() && check_backend_with(client, base_url, model, api_key, true).await
}

fn spawn_sidecar() -> std::io::Result<tokio::process::Child> {
    let data_dir = config::data_dir();
    std::fs::create_dir_all(&data_dir).ok();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("apfel.log"))
        .ok();

    let mut cmd = tokio::process::Command::new("apfel");
    cmd.args([
        "--serve",
        "--host",
        APFEL_HOST,
        "--port",
        &APFEL_PORT.to_string(),
    ])
    .stdin(Stdio::null())
    .kill_on_drop(true);
    match log.and_then(|file| file.try_clone().ok()) {
        Some(out) => match out.try_clone() {
            Ok(err) => {
                cmd.stdout(out).stderr(err);
            }
            Err(_) => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        },
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    cmd.spawn()
}

fn not_ready_message(was_already_running: bool) -> String {
    let prefix = if was_already_running {
        "the owned Apfel backend"
    } else {
        "the spawned `apfel --serve`"
    };
    format!(
        "{prefix} on {APFEL_HOST}:{APFEL_PORT} did not serve model `{APFEL_MODEL}` \
         within 60s. If Apple Intelligence is unavailable on this Mac, enable it in \
         System Settings > Apple Intelligence & Siri (Apple Silicon, macOS 26+ required), \
         then run `awpfel start` again. Details may be in {}",
        config::data_dir().join("apfel.log").display()
    )
}

pub fn sidecar_pid_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    config::runtime_dir().join(format!("awpfel-apfel-{uid}.pid"))
}

/// Ownership record: pid plus process-start identity plus argv0. A pid alone
/// can be recycled; the triple must match before any signal is sent.
struct SidecarRecord {
    pid: u32,
    started: String,
    argv0: String,
}

impl SidecarRecord {
    fn capture(pid: u32) -> Option<Self> {
        Some(Self {
            pid,
            started: process_start(pid)?,
            argv0: process_argv(pid)?.into_iter().next()?,
        })
    }

    fn serialize(&self) -> String {
        format!("{}\n{}\n{}\n", self.pid, self.started, self.argv0)
    }

    fn parse(content: &str) -> Option<Self> {
        let mut lines = content.lines();
        Some(Self {
            pid: lines.next()?.trim().parse().ok()?,
            started: lines.next()?.to_string(),
            argv0: lines.next()?.to_string(),
        })
    }
}

fn read_sidecar_record() -> Option<SidecarRecord> {
    SidecarRecord::parse(&std::fs::read_to_string(sidecar_pid_path()).ok()?)
}

fn write_sidecar_record(content: &str) {
    let path = sidecar_pid_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, content).ok();
}

fn remove_sidecar_pid() {
    std::fs::remove_file(sidecar_pid_path()).ok();
}

/// Remove the record only if it still contains exactly what we wrote (we hold
/// the startup lock, but never unlink another owner's state on principle).
fn remove_sidecar_record_if(written: &str) {
    let current = std::fs::read_to_string(sidecar_pid_path()).unwrap_or_default();
    if current == written {
        std::fs::remove_file(sidecar_pid_path()).ok();
    }
}

/// Whether the record still denotes the same live process we recorded: alive,
/// same process-start identity, and an argv that is exactly our sidecar
/// invocation. Anything else — dead, recycled pid, foreign binary — returns
/// false and must never be signaled.
fn verify_record(record: &SidecarRecord) -> bool {
    if record.pid == 0 || !pid_alive(record.pid) {
        return false;
    }
    match (process_start(record.pid), process_argv(record.pid)) {
        (Some(started), Some(argv)) => {
            started == record.started
                && argv.first().is_some_and(|first| first == &record.argv0)
                && argv_is_owned_sidecar(&argv)
        }
        _ => false,
    }
}

/// Stop the sidecar recorded in the pid file. Returns true when no verified
/// owned process remains (stopped, already gone, or never verifiable — which
/// is left strictly alone). Returns false only when a *verified* owned
/// process could not be stopped; the record is kept for retry and the caller
/// must report the failure.
pub async fn stop_owned_sidecar() -> bool {
    let Some(record) = read_sidecar_record() else {
        return true;
    };
    if record.pid == 0 || !pid_alive(record.pid) {
        remove_sidecar_pid();
        return true;
    }
    if !verify_record(&record) {
        // Alive but not provably ours: never touch, never unlink.
        return true;
    }
    signal_verified_sidecar(&record).await
}

async fn signal_verified_sidecar(record: &SidecarRecord) -> bool {
    let pid = record.pid;
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let start = Instant::now();
    while pid_alive(pid) && start.elapsed() < STOP_DEADLINE {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !pid_alive(pid) {
        remove_sidecar_pid();
        return true;
    }
    // Revalidate identity immediately before escalating: the process may have
    // exited and the pid been recycled during the wait.
    match read_sidecar_record() {
        Some(current) if verify_record(&current) && current.pid == pid => unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        },
        _ => return false,
    }
    let start = Instant::now();
    while pid_alive(pid) && start.elapsed() < STOP_DEADLINE {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if pid_alive(pid) {
        return false;
    }
    remove_sidecar_pid();
    true
}

/// Whether `pid` denotes a live process. `main.rs` needs this for the daemon
/// pid-file sanity check (stale/recycled pid files must never be signaled).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Exact argv tokens of `pid` via `ps`. `None` when the process is gone or
/// `ps` is unavailable — callers treat that as unverifiable, never as owned.
pub fn process_argv(pid: u32) -> Option<Vec<String>> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let args = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if args.is_empty() {
        return None;
    }
    Some(args.split_whitespace().map(str::to_string).collect())
}

/// Process-start identity of `pid` via `ps lstart`. Combined with the pid and
/// exact argv, this defeats pid-reuse mistakes: a recycled pid has a
/// different start time.
fn process_start(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if started.is_empty() {
        return None;
    }
    Some(started)
}

/// Whether `argv` is exactly our sidecar invocation: an executable named
/// `apfel` serving the dedicated loopback port. Token-exact, so unrelated
/// processes merely mentioning the words (`python apfel_monitor.py --serve`,
/// an editor with such a file open) never match.
pub fn argv_is_owned_sidecar(argv: &[String]) -> bool {
    let [first, rest @ ..] = argv else {
        return false;
    };
    if first.rsplit('/').next().unwrap_or(first) != "apfel" {
        return false;
    }
    let has = |flag: &str| rest.iter().any(|arg| arg == flag);
    has("--serve")
        && has("--host")
        && has(APFEL_HOST)
        && has("--port")
        && has(&APFEL_PORT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APFEL_MODELS_DOC: &str = r#"{
  "data" : [
    {
      "context_window" : 4096,
      "created" : 1719792000,
      "id" : "apple-foundationmodel",
      "object" : "model",
      "owned_by" : "apple"
    }
  ],
  "object" : "list"
}"#;

    const APFEL_HEALTH_DOC: &str = r#"{"active_requests":0,"context_window":4096,"model":"apple-foundationmodel","model_available":true,"prewarmed":true,"status":"ok","version":"1.16.0"}"#;

    #[test]
    fn models_identity_matches_apfel_document() {
        assert!(backend_serves_model(APFEL_MODELS_DOC, APFEL_MODEL));
    }

    #[test]
    fn models_identity_rejects_foreign_server() {
        let ollama = r#"{"object":"list","data":[{"id":"llama3.1:8b","object":"model"}]}"#;
        assert!(!backend_serves_model(ollama, APFEL_MODEL));
        assert!(!backend_serves_model("not json at all", APFEL_MODEL));
        assert!(!backend_serves_model("{}", APFEL_MODEL));
        assert!(!backend_serves_model(r#"{"data":[]}"#, APFEL_MODEL));
    }

    #[test]
    fn health_gate_requires_model_available() {
        assert!(health_reports_available(APFEL_HEALTH_DOC, APFEL_MODEL));
        let unavailable =
            APFEL_HEALTH_DOC.replace("\"model_available\":true", "\"model_available\":false");
        assert!(!health_reports_available(&unavailable, APFEL_MODEL));
        assert!(!health_reports_available(r#"{"status":"ok"}"#, APFEL_MODEL));
        assert!(!health_reports_available("not json", APFEL_MODEL));
        let wrong_model = APFEL_HEALTH_DOC.replace("apple-foundationmodel", "other-model");
        assert!(!health_reports_available(&wrong_model, APFEL_MODEL));
    }

    #[test]
    fn sidecar_argv_matches_only_dedicated_invocation() {
        let owned = [
            "/opt/homebrew/bin/apfel",
            "--serve",
            "--host",
            "127.0.0.1",
            "--port",
            "11435",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
        assert!(argv_is_owned_sidecar(&owned));
        // Lookalike processes merely mentioning the words must never match.
        for foreign in [
            vec![
                "python",
                "/tmp/apfel_monitor.py",
                "--serve",
                "--port",
                "11435",
            ],
            vec!["vim", "apfel --serve --host 127.0.0.1 --port 11435 notes"],
            vec!["apfel", "hello"],
            vec![
                "/opt/homebrew/bin/apfel",
                "--serve",
                "--host",
                "127.0.0.1",
                "--port",
                "11434",
            ],
            vec!["ollama", "serve"],
        ] {
            let argv = foreign.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert!(!argv_is_owned_sidecar(&argv), "{foreign:?}");
        }
        assert!(!argv_is_owned_sidecar(&[]));
    }

    #[test]
    fn managed_endpoint_detection() {
        assert!(is_managed_endpoint("http://127.0.0.1:11435/v1"));
        assert!(is_managed_endpoint("http://127.0.0.1:11435/v1/"));
        assert!(is_managed_endpoint("http://localhost:11435/v1"));
        // Substring lookalikes and wrong ports/paths are not managed.
        assert!(!is_managed_endpoint("http://127.0.0.1:11434/v1"));
        assert!(!is_managed_endpoint("https://api.example.com/v1"));
        assert!(!is_managed_endpoint(
            "http://127.0.0.1:11435.evil.example/v1"
        ));
        assert!(!is_managed_endpoint(
            "http://evil.example/?x=127.0.0.1:11435"
        ));
        assert!(!is_managed_endpoint("http://evil.example:11435/v1"));
        assert!(!is_managed_endpoint("http://127.0.0.1:11435"));
        assert!(!is_managed_endpoint("not a url"));
    }

    #[test]
    fn health_url_targets_server_root() {
        let url = health_url("http://127.0.0.1:11435/v1").unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:11435/health");
    }
}
