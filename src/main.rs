use awpfel::config;
use awpfel::daemon;
use awpfel::layer::history::HistoryLayer;
use awpfel::layer::history_import;
use awpfel::protocol;

mod apfel;

use apfel::{CancelFlag, EnsureOutcome};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "awpfel",
    version,
    about = "Awpfel — on-device AI ghost text for your shell, backed by a managed local Apfel."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the Awpfel daemon (ensures the owned Apfel backend first)
    Start,
    /// Stop the Awpfel daemon and its owned Apfel backend
    Stop,
    /// Show daemon status
    Status,
    /// Show daemon logs
    Logs {
        /// Number of lines to show
        #[arg(short, long, default_value_t = 50)]
        lines: usize,
    },
    /// Open or show configuration
    Config,
    /// Show current context engine state
    Context,
    /// Configure shell integration (add source line to $ZDOTDIR/.zshrc or ~/.zshrc)
    Setup {
        /// Skip confirmation before modifying the shell rc file
        #[arg(long)]
        yes: bool,
    },
    /// Manage command history
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },
    /// Remove all Awpfel data, config, and shell integration
    Uninstall {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum HistoryAction {
    /// Import commands from zsh history file
    Import {
        /// Path to history file (default: $HISTFILE or ~/.zsh_history)
        #[arg(long)]
        file: Option<PathBuf>,
        /// Force import even if database is not empty
        #[arg(long)]
        force: bool,
    },
    /// Show history database statistics
    Stats,
    /// Clear all history entries
    Clear {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let code = match cli.command {
        Commands::Start => cmd_start().await,
        Commands::Stop => cmd_stop().await,
        Commands::Status => cmd_status().await,
        Commands::Logs { lines } => {
            cmd_logs(lines);
            0
        }
        Commands::Config => {
            cmd_config();
            0
        }
        Commands::Context => cmd_context().await,
        Commands::Setup { yes } => cmd_setup(yes).await,
        Commands::History { action } => match action {
            HistoryAction::Import { file, force } => cmd_history_import(file, force),
            HistoryAction::Stats => {
                cmd_history_stats();
                0
            }
            HistoryAction::Clear { yes } => cmd_history_clear(yes),
        },
        Commands::Uninstall { yes } => cmd_uninstall(yes).await,
    };
    std::process::exit(code);
}

async fn cmd_start() -> i32 {
    // Serialize concurrent startups (fresh-boot shells, setup races) so only
    // one process can spawn the owned Apfel sidecar. The OS advisory lock is
    // held for the daemon's whole lifetime (never unlinked while live): this
    // also serializes the stale-socket removal + bind in daemon::run, which
    // the old early-drop guard left racy. `setup` stops the running daemon
    // (releasing its lock) before restarting, so this cannot deadlock.
    let _start_lock = match acquire_start_lock() {
        LockOutcome::Acquired(guard) => guard,
        LockOutcome::Busy => {
            eprintln!("awpfel: another `awpfel start` is already in progress");
            return 1;
        }
    };

    if daemon::is_running() {
        eprintln!("awpfel daemon is already running");
        return 1;
    }

    // Persistent cancellation across backend ensure AND daemon serving. Signal
    // handlers are registered before anything is spawned: a SIGTERM/SIGINT
    // during the readiness wait kills and reaps the partial sidecar instead
    // of orphaning it, and a signal during serving drops the daemon future
    // (aborting the listener) with explicit cleanup below. A one-shot
    // self-IPC Shutdown request would be lost if it arrived before the bind,
    // so the flag — not socket IPC — is the cancellation channel.
    let cancel = Arc::new(CancelFlag::default());
    {
        let cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    {
        let cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            let mut sig =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(sig) => sig,
                    Err(_) => return,
                };
            if sig.recv().await.is_some() {
                cancel.cancel();
            }
        });
    }

    let config = config::load_config();

    std::fs::create_dir_all(config::data_dir()).ok();
    std::fs::create_dir_all(config::config_dir()).ok();

    let log_path = config::log_path();
    let log_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!(
                "awpfel: failed to open log file {}: {e}",
                log_path.display()
            );
            return 1;
        }
    };

    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("awpfel daemon starting");

    // Fresh defaults must work with no custom config or manual server
    // commands: ensure the managed Apfel backend before serving. An
    // unavailable backend is a visible error, never a silent fallback or an
    // infinite retry loop.
    let api_key = if config.ai.api_key.is_empty() {
        None
    } else {
        Some(config.ai.api_key.as_str())
    };
    let owned_sidecar = if config.ai.enabled {
        match apfel::ensure_local_apfel(&config.ai.base_url, &config.ai.model, api_key, &cancel)
            .await
        {
            EnsureOutcome::HealthyExisting => {
                println!(
                    "awpfel: using existing apfel backend ({})",
                    config.ai.base_url
                );
                None
            }
            EnsureOutcome::StartedOwned(owned) => {
                println!(
                    "awpfel: apfel backend ready on {}:{}",
                    apfel::APFEL_HOST,
                    apfel::APFEL_PORT
                );
                Some(owned)
            }
            EnsureOutcome::Unavailable(message) => {
                eprintln!("awpfel: AI backend unavailable; daemon not started.\n\n{message}");
                return 1;
            }
            EnsureOutcome::Cancelled => {
                eprintln!("awpfel: interrupted during backend startup; daemon not started");
                return 130;
            }
        }
    } else {
        tracing::info!("ai.enabled is false; skipping apfel backend check");
        None
    };

    if cancel.is_cancelled() {
        if let Some(owned) = owned_sidecar {
            tracing::info!("interrupted before serving; stopping owned apfel sidecar");
            owned.stop().await;
        }
        eprintln!("awpfel: interrupted before serving; daemon not started");
        return 130;
    }

    println!("awpfel daemon starting...");

    // The daemon future is fused with the persistent cancel flag: on signal
    // the listener is dropped (accept loop aborts) and we fall through to
    // the same explicit cleanup (socket/pid files, owned sidecar) that the
    // graceful path performs. Dropping is safe: daemon::run removes its
    // files only after the loop breaks, so anything left behind is removed
    // here. Shutdown-via-socket still works normally when no signal arrives.
    let mut daemon_done = tokio::spawn(daemon::run(config));
    tokio::select! {
        _ = cancel.cancelled() => {
            daemon_done.abort();
            let _ = daemon_done.await;
        }
        _ = &mut daemon_done => {}
    }

    // Same cleanup `daemon::run` performs on its graceful path, so a signal
    // mid-serve leaves no stale socket/pid behind.
    daemon::cleanup_socket();
    if let Some(owned) = owned_sidecar {
        tracing::info!("stopping owned apfel sidecar");
        owned.stop().await;
    }
    if cancel.is_cancelled() {
        return 130;
    }
    0
}

async fn cmd_stop() -> i32 {
    if !daemon::is_running() {
        println!("awpfel daemon is not running");
    } else if stop_daemon_gracefully().await {
        println!("awpfel daemon stopped");
    } else {
        eprintln!("awpfel: failed to stop daemon");
        return 1;
    }
    // Stop the sidecar this daemon recorded. Verification happens inside
    // (only a provably owned process is ever signaled); a failure is a real
    // error, not a silent success.
    if apfel::sidecar_pid_path().exists() {
        if apfel::stop_owned_sidecar().await {
            println!("awpfel: owned apfel backend stopped");
        } else {
            eprintln!("awpfel: failed to stop the owned apfel backend; see logs");
            return 1;
        }
    }
    0
}

async fn cmd_status() -> i32 {
    if !daemon::is_running() {
        println!("awpfel daemon is not running");
        return 0;
    }

    match daemon::send_status_request().await {
        Ok(protocol::Response::Status(s)) => {
            println!("awpfel daemon is running (pid: {})", s.pid);
            println!("  uptime: {}s", s.uptime_secs);
            println!("  history entries: {}", s.history_count);
            println!("  AI enabled: {}", s.ai_enabled);
        }
        Ok(_) => println!("awpfel daemon is running"),
        Err(e) => {
            eprintln!("failed to query status: {e}");
            return 1;
        }
    }

    // Backend health: models identity plus /health model_available, never
    // starts anything. Skip the side effect of creating a default config
    // when none exists.
    if config::config_dir().join("config.toml").exists() {
        let config = config::load_config();
        if config.ai.enabled {
            let api_key = if config.ai.api_key.is_empty() {
                None
            } else {
                Some(config.ai.api_key.as_str())
            };
            let managed = apfel::is_managed_endpoint(&config.ai.base_url);
            if apfel::check_backend(&config.ai.base_url, &config.ai.model, api_key, managed).await {
                println!("  apfel backend: healthy ({})", config.ai.base_url);
            } else {
                println!("  apfel backend: unreachable ({})", config.ai.base_url);
            }
        }
    }
    0
}

fn cmd_logs(lines: usize) {
    let log_path = config::log_path();
    if !log_path.exists() {
        println!("no log file found at {}", log_path.display());
        return;
    }
    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            let all_lines: Vec<&str> = content.lines().collect();
            let start = all_lines.len().saturating_sub(lines);
            for line in &all_lines[start..] {
                println!("{line}");
            }
        }
        Err(e) => eprintln!("failed to read logs: {e}"),
    }
}

fn cmd_config() {
    let config_path = config::config_dir().join("config.toml");
    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => print!("{content}"),
            Err(e) => eprintln!("failed to read config: {e}"),
        }
    } else {
        let default = config::AwpfelConfig::default();
        let content = toml::to_string_pretty(&default).unwrap();
        println!("# No config file found. Default config:");
        println!("# Create at: {}", config_path.display());
        println!();
        print!("{content}");
    }
}

fn cmd_history_import(file: Option<PathBuf>, force: bool) -> i32 {
    let db_path = config::history_db_path();

    if db_path.exists()
        && !force
        && let Ok(history) = HistoryLayer::new(&db_path)
    {
        let count = history.count();
        if count > 0 {
            eprintln!(
                "history database already has {count} entries. Use --force to import anyway."
            );
            return 1;
        }
    }

    let histfile = file.unwrap_or_else(config::default_zsh_histfile);
    if !histfile.exists() {
        eprintln!("history file not found: {}", histfile.display());
        return 1;
    }

    std::fs::create_dir_all(config::data_dir()).ok();
    println!("importing from {}...", histfile.display());

    match history_import::import_zsh_history(&db_path, &histfile) {
        Ok(r) => {
            println!("import complete:");
            println!("  total entries parsed: {}", r.total_lines);
            println!("  imported:             {}", r.imported);
            println!("  skipped (sensitive):  {}", r.skipped_sensitive);
            println!("  skipped (empty):      {}", r.skipped_empty);
            0
        }
        Err(e) => {
            eprintln!("import failed: {e}");
            1
        }
    }
}

fn cmd_history_stats() {
    let db_path = config::history_db_path();
    if !db_path.exists() {
        println!("no history database found");
        return;
    }
    match HistoryLayer::new(&db_path) {
        Ok(history) => {
            let count = history.count();
            let size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
            println!("history database: {}", db_path.display());
            println!("  entries: {count}");
            println!("  size:    {:.1} KB", size as f64 / 1024.0);
        }
        Err(e) => eprintln!("failed to open history: {e}"),
    }
}

fn cmd_history_clear(yes: bool) -> i32 {
    let db_path = config::history_db_path();
    if !db_path.exists() {
        println!("no history database found");
        return 0;
    }

    if !yes {
        eprint!("this will delete all history entries. continue? [y/N] ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            println!("aborted");
            return 0;
        }
    }

    match std::fs::remove_file(&db_path) {
        Ok(()) => {
            println!("history cleared");
            0
        }
        Err(e) => {
            eprintln!("failed to clear history: {e}");
            1
        }
    }
}

async fn cmd_setup(yes: bool) -> i32 {
    let config_dir = config::config_dir();
    let data_dir = config::data_dir();
    std::fs::create_dir_all(&config_dir).ok();
    std::fs::create_dir_all(&data_dir).ok();

    // 1. Config: defaults already point at the managed local Apfel, so a
    // fresh setup works with no manual server commands or API keys.
    let config_path = config_dir.join("config.toml");
    let had_config = config_path.exists();
    config::load_config();
    if had_config {
        println!("[1/4] config: {}", config_path.display());
    } else {
        println!("[1/4] config: created {}", config_path.display());
    }

    // 2. Plugin + shell integration.
    let plugin_path = match find_plugin() {
        Some(path) => path,
        None => {
            eprintln!("error: could not find awpfel.zsh plugin");
            eprintln!("  checked: <install prefix>/share/awpfel/awpfel.zsh");
            eprintln!("  checked: {}", config_dir.join("awpfel.zsh").display());
            eprintln!(
                "  if you installed from source, copy plugin/awpfel*.zsh into {}",
                config_dir.display()
            );
            return 1;
        }
    };
    let zshrc = zshrc_path();
    match ensure_shell_integration(&zshrc, &plugin_path, yes) {
        Ok(ShellOutcome::Added) => {
            println!("[2/4] shell: added source line to {}", zshrc.display())
        }
        Ok(ShellOutcome::AlreadyConfigured) => {
            println!("[2/4] shell: already configured in {}", zshrc.display())
        }
        Ok(ShellOutcome::Declined) => {
            println!("[2/4] shell: skipped (answer was not 'y')");
            println!();
            println!("to configure manually, add this to your shell rc file:");
            println!("  {}", source_line_for(&plugin_path));
            println!("and to auto-start the daemon on new shells:");
            println!("  {AUTOSTART_BEGIN} ... {AUTOSTART_END} (see `awpfel setup --yes`)");
        }
        Err(message) => {
            eprintln!("[2/4] shell: {message}");
            return 1;
        }
    }

    // 3. Restart the daemon detached from this terminal. `awpfel start`
    // ensures the owned Apfel sidecar itself; the previous sidecar (if any)
    // is fully torn down first (stop waits for the old daemon's sidecar
    // record to clear), so the replacement never adopts a backend that is
    // about to be killed.
    if daemon::is_running() && !stop_daemon_gracefully().await {
        eprintln!("[3/4] daemon: failed to stop the previous daemon");
        return 1;
    }
    let mut child = match spawn_detached_daemon() {
        Ok(child) => child,
        Err(message) => {
            eprintln!("[3/4] daemon: {message}");
            return 1;
        }
    };
    println!("[3/4] daemon: starting in background...");

    // 4. Verify with a real status response (bounded wait; fail fast if the
    // child exits). No false success: anything else is an error.
    match wait_for_daemon_ready(&mut child, Duration::from_secs(90)).await {
        Ok(s) => {
            println!(
                "[4/4] status: pid {}, {} history entries, AI enabled: {}",
                s.pid, s.history_count, s.ai_enabled
            );
        }
        Err(message) => {
            eprintln!("[4/4] status: daemon did not become ready: {message}");
            eprint_log_tail();
            return 1;
        }
    }

    println!();
    println!(
        "awpfel setup complete! restart your shell or run: source {}",
        zshrc.display()
    );
    0
}

/// Wait for a status response from the freshly spawned daemon. Fails fast if
/// the child exits first (e.g. the Apfel backend is unavailable), otherwise
/// waits up to `deadline` for slow first-time model loads.
async fn wait_for_daemon_ready(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Result<protocol::StatusResponse, String> {
    let start = Instant::now();
    loop {
        match daemon::send_status_request().await {
            Ok(protocol::Response::Status(s)) => return Ok(s),
            Ok(_) => {}
            Err(_) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "daemon process exited ({status}) before serving requests"
                ));
            }
            Ok(None) => {}
            Err(e) => {
                return Err(format!("failed to poll daemon process: {e}"));
            }
        }
        if start.elapsed() >= deadline {
            child.kill().ok();
            return Err(format!("no status response within {}s", deadline.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn spawn_detached_daemon() -> Result<std::process::Child, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("failed to locate awpfel binary: {e}"))?;
    // Detached children lose their terminal streams; point stderr at the log
    // so early startup errors (e.g. backend unavailable) stay visible via
    // `awpfel logs` and the setup failure report.
    let log_path = config::log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open log file {}: {e}", log_path.display()))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| format!("failed to start daemon: {e}"))
}

fn eprint_log_tail() {
    let log_path = config::log_path();
    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return;
    }
    let start = lines.len().saturating_sub(15);
    eprintln!("--- last log lines ({}):", log_path.display());
    for line in &lines[start..] {
        eprintln!("  {line}");
    }
}

/// Ask a running daemon to shut down via its socket, falling back to a
/// *verified* SIGTERM. A stale pid file whose number has been recycled by an
/// unrelated process must never be signaled. Shutdown is complete only when
/// the *verified old daemon process itself* has exited (not merely when the
/// socket disappears): the daemon removes its socket before stopping its
/// owned sidecar, and a replacement started too early could reuse a backend
/// the old daemon is still about to kill. Returns true when the old daemon
/// is gone.
///
/// No separate wait on the sidecar record: the old daemon holds the startup
/// lock for its whole lifetime and stops its owned sidecar before exiting,
/// so process exit already implies its teardown finished — and the lifetime
/// lock keeps any replacement from starting until then. A leftover record
/// (old daemon SIGKILLed, sidecar still healthy) must NOT block the restart:
/// the replacement safely reuses that backend without ever signaling it.
async fn stop_daemon_gracefully() -> bool {
    let old_pid = daemon::read_pid();
    if !daemon::is_running() {
        return true;
    }
    daemon::send_shutdown().await.ok();
    if !wait_old_daemon_gone(old_pid, Duration::from_secs(4)).await {
        match daemon::read_pid() {
            Some(pid) if daemon_pid_is_ours(pid) => unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            },
            Some(_) => {
                eprintln!("awpfel: pid file is stale; not signaling an unverified process");
            }
            None => {}
        }
        if !wait_old_daemon_gone(daemon::read_pid().or(old_pid), Duration::from_secs(4)).await {
            return false;
        }
    }
    daemon::cleanup_socket();
    std::fs::remove_file(config::pid_path()).ok();
    true
}

/// Wait until the verified old daemon process has exited. `old_pid` pins the
/// generation: if the pid file is missing or recycled we fall back to the
/// socket check, but a live verified pid keeps us waiting for *that process*.
async fn wait_old_daemon_gone(old_pid: Option<u32>, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        match old_pid {
            Some(pid) if daemon_pid_is_ours(pid) => {
                if !apfel::pid_alive(pid) {
                    return true;
                }
            }
            Some(_) => {
                // Pid file stale or recycled: nothing verified to wait on.
                if !daemon::is_running() {
                    return true;
                }
            }
            None => {
                if !daemon::is_running() {
                    return true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match old_pid {
        Some(pid) if daemon_pid_is_ours(pid) => !apfel::pid_alive(pid),
        _ => !daemon::is_running(),
    }
}

/// Whether `pid` may be signaled as our daemon: alive, not ourselves, and its
/// exact argv is an awpfel invocation (executable name plus `start`). A
/// substring match would accept e.g. an editor holding a file whose path
/// mentions awpfel; anything unverifiable is treated as "not ours".
fn daemon_pid_is_ours(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    if !apfel::pid_alive(pid) {
        return false;
    }
    match apfel::process_argv(pid) {
        Some(argv) => daemon_argv_is_awpfel(&argv),
        None => false,
    }
}

fn daemon_argv_is_awpfel(argv: &[String]) -> bool {
    let [first, rest @ ..] = argv else {
        return false;
    };
    if first.rsplit('/').next().unwrap_or(first) != "awpfel" {
        return false;
    }
    rest.iter().any(|arg| arg == "start")
}

/// OS advisory lock for daemon startup, held for the daemon's whole lifetime.
/// Concurrent `awpfel start` invocations (fresh-boot shells, setup races) must
/// not spawn competing Apfel sidecars; losers exit and rely on the winner's
/// socket. `flock(LOCK_EX|LOCK_NB)` on a stable lock file: no PID contents,
/// no unlinking while live, no stale-recovery heuristics — the OS releases
/// the lock on process exit, so a SIGKILLed starter can never wedge future
/// starts. The file itself is never removed while a holder may be live.
struct StartLock {
    _file: std::fs::File,
}

enum LockOutcome {
    Acquired(StartLock),
    Busy,
}

fn start_lock_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    config::runtime_dir().join(format!("awpfel-start-{uid}.lock"))
}

fn acquire_start_lock() -> LockOutcome {
    let path = start_lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) => {
            // Without a writable runtime dir the socket bind below cannot
            // succeed either; fail here instead of reporting false success.
            eprintln!("awpfel: cannot open startup lock {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    // SAFETY: flock on our own open file descriptor; no shared memory.
    let locked = unsafe { libc::flock(file_as_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if locked {
        LockOutcome::Acquired(StartLock { _file: file })
    } else {
        LockOutcome::Busy
    }
}

#[cfg(unix)]
fn file_as_fd(file: &std::fs::File) -> libc::c_int {
    use std::os::unix::io::AsRawFd as _;
    file.as_raw_fd()
}

#[cfg(not(unix))]
fn file_as_fd(_file: &std::fs::File) -> libc::c_int {
    -1
}

fn find_plugin() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(prefix) = exe.parent().and_then(|p| p.parent())
    {
        let brew_plugin = prefix.join("share/awpfel/awpfel.zsh");
        if brew_plugin.exists() {
            return Some(brew_plugin);
        }
    }
    let local = config::config_dir().join("awpfel.zsh");
    if local.exists() {
        return Some(local);
    }
    None
}

/// Shell rc file to integrate with: `$ZDOTDIR/.zshrc` when ZDOTDIR is set,
/// otherwise `~/.zshrc`. Both halves respect a sandboxed `$HOME`.
fn zshrc_path() -> PathBuf {
    if let Ok(zdotdir) = std::env::var("ZDOTDIR")
        && !zdotdir.is_empty()
    {
        return PathBuf::from(zdotdir).join(".zshrc");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".zshrc")
}

/// Managed integration markers. Setup writes exactly one source line
/// (`source_line_for`, safely quoted) plus one autostart block wrapped in the
/// paired `AUTOSTART_BEGIN/END` markers. Removal recognizes *only* these
/// exact lines: an independent upstream Awen install, a different awpfel
/// prefix, a commented mention (`# awpfel.zsh notes`), or a similarly named
/// file (`awpfel.zsh.local`) is never touched — awpfel upgrades across
/// prefixes by replacing only its own previously written exact line.
const AUTOSTART_BEGIN: &str = "# Awpfel autostart (begin)";
const AUTOSTART_END: &str = "# Awpfel autostart (end)";
const SOURCE_HEADER: &str = "# Awpfel — on-device AI shell assistance";

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn source_line_for(plugin: &Path) -> String {
    format!("source {}", shell_quote(&plugin.to_string_lossy()))
}

/// Auto-start block: recovers the daemon (and its owned backend) on future
/// sessions. `awpfel start` fast-paths when already running and serializes
/// concurrent startups, so this is cheap and race-free.
fn autostart_block() -> String {
    format!(
        "\n{AUTOSTART_BEGIN}\n\
         # Start the awpfel daemon (and its owned Apfel backend) if needed.\n\
         if command -v awpfel >/dev/null 2>&1; then\n  \
         awpfel start >/dev/null 2>&1 &!\n\
         fi\n\
         {AUTOSTART_END}\n"
    )
}

enum ShellOutcome {
    Added,
    AlreadyConfigured,
    Declined,
}

/// Append the quoted source line plus the autostart block idempotently.
/// Only the exact wanted source line is preserved; no other `source` line is
/// ever removed or altered (conservative: an unknown line might be an
/// independent install or user content). An existing autostart block is left
/// untouched.
fn ensure_shell_integration(
    zshrc: &Path,
    plugin: &Path,
    yes: bool,
) -> Result<ShellOutcome, String> {
    let wanted_source = source_line_for(plugin);
    let existing = std::fs::read_to_string(zshrc).unwrap_or_default();
    let has_autostart = has_paired_autostart_block(&existing);
    let has_exact_source = existing
        .lines()
        .any(|line| line.trim() == wanted_source.trim());
    if has_exact_source && has_autostart {
        return Ok(ShellOutcome::AlreadyConfigured);
    }

    if !yes {
        eprint!(
            "[2/4] shell: add source line to {}? [y/N] ",
            zshrc.display()
        );
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            return Ok(ShellOutcome::Declined);
        }
    }

    let mut new_content = existing.clone();
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    let mut changed = false;
    if !has_exact_source {
        new_content.push_str(&format!("{SOURCE_HEADER}\n{wanted_source}\n"));
        changed = true;
    }
    if !has_autostart {
        new_content.push_str(&autostart_block());
        changed = true;
    }
    if !changed {
        return Ok(ShellOutcome::AlreadyConfigured);
    }

    if let Some(parent) = zshrc.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    std::fs::write(zshrc, new_content)
        .map_err(|e| format!("failed to write to {}: {e}", zshrc.display()))?;
    Ok(ShellOutcome::Added)
}

/// Whether the rc file contains a complete paired autostart block. An
/// unmatched BEGIN without its END is not a block: setup adds nothing and
/// uninstall removes nothing, preserving whatever follows.
fn has_paired_autostart_block(content: &str) -> bool {
    let mut seen_begin = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == AUTOSTART_BEGIN {
            seen_begin = true;
        } else if trimmed == AUTOSTART_END && seen_begin {
            return true;
        }
    }
    false
}

/// Remove exactly what `ensure_shell_integration` manages: the paired
/// autostart block (BEGIN..END, only when both markers are present), our
/// header line, and the exact known source lines awpfel itself installs
/// (reconstructed from the install prefix and config-dir fallback, since the
/// plugin path may already be gone at uninstall time). Everything else —
/// upstream Awen lines, similarly named files, comments, user content — is
/// preserved verbatim. Returns true when the file was modified.
fn strip_shell_integration(zshrc: &Path) -> bool {
    strip_shell_integration_known(zshrc, &known_source_lines())
}

fn strip_shell_integration_known(zshrc: &Path, known_sources: &[String]) -> bool {
    let content = match std::fs::read_to_string(zshrc) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut remove = vec![false; lines.len()];
    let mut changed = false;
    // Paired block removal only: an unmatched BEGIN preserves the marker and
    // everything after it.
    let mut begin_at: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == AUTOSTART_BEGIN {
            begin_at = Some(index);
        } else if trimmed == AUTOSTART_END
            && let Some(start) = begin_at.take()
        {
            for flag in &mut remove[start..=index] {
                *flag = true;
            }
            changed = true;
        }
    }
    for (index, line) in lines.iter().enumerate() {
        if remove[index] {
            continue;
        }
        let trimmed = line.trim();
        if trimmed == SOURCE_HEADER || known_sources.iter().any(|known| trimmed == known.trim()) {
            remove[index] = true;
            changed = true;
        }
    }
    if !changed {
        return false;
    }
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !remove[*index])
        .map(|(_, line)| *line)
        .collect();
    let mut new_content = kept.join("\n");
    new_content.push('\n');
    std::fs::write(zshrc, new_content).is_ok()
}

async fn cmd_uninstall(yes: bool) -> i32 {
    if !yes {
        eprintln!("this will remove all Awpfel data, config, and shell integration.");
        eprint!("continue? [y/N] ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            println!("aborted");
            return 0;
        }
    }

    // 1. Stop daemon plus the sidecar it recorded (only a provably owned
    // process is ever signaled; externally started Apfel and any global
    // Apfel/Ollama are never touched).
    if daemon::is_running() {
        if stop_daemon_gracefully().await {
            println!("[1/4] daemon: stopped");
        } else {
            eprintln!("[1/4] daemon: failed to stop; continuing cleanup");
        }
    } else {
        println!("[1/4] daemon: not running");
    }
    if apfel::sidecar_pid_path().exists() {
        if apfel::stop_owned_sidecar().await {
            println!("[1/4] apfel: owned backend stopped");
        } else {
            eprintln!("[1/4] apfel: failed to stop the owned backend; continuing cleanup");
        }
    }

    // 2. Remove shell integration. The plugin path may already be gone, so
    // the exact known source line is reconstructed from both the install
    // prefix layout and the config-dir fallback; anything not positively
    // identified as ours is preserved.
    let zshrc = zshrc_path();
    if zshrc.exists() {
        if strip_shell_integration(&zshrc) {
            println!("[2/4] shell: removed integration from {}", zshrc.display());
        } else {
            println!("[2/4] shell: no awpfel lines found in {}", zshrc.display());
        }
    } else {
        println!("[2/4] shell: {} not found", zshrc.display());
    }

    // 3. Remove config directory
    let config_dir = config::config_dir();
    if config_dir.exists() {
        match std::fs::remove_dir_all(&config_dir) {
            Ok(()) => println!("[3/4] config: removed {}", config_dir.display()),
            Err(e) => eprintln!(
                "[3/4] config: failed to remove {}: {e}",
                config_dir.display()
            ),
        }
    } else {
        println!("[3/4] config: {} not found", config_dir.display());
    }

    // 4. Remove data directory
    let data_dir = config::data_dir();
    if data_dir.exists() {
        match std::fs::remove_dir_all(&data_dir) {
            Ok(()) => println!("[4/4] data: removed {}", data_dir.display()),
            Err(e) => eprintln!("[4/4] data: failed to remove {}: {e}", data_dir.display()),
        }
    } else {
        println!("[4/4] data: {} not found", data_dir.display());
    }

    // Cleanup runtime files. The startup lock file is intentionally left in
    // place: the advisory lock requires a stable inode, and a stale file is
    // harmless (the OS lock releases on exit). Unlinking here could race a
    // concurrent autostart into locking a new inode while an old holder still
    // runs.
    std::fs::remove_file(config::socket_path()).ok();
    std::fs::remove_file(config::pid_path()).ok();

    println!();
    println!("awpfel uninstalled. if installed via brew, also run:");
    println!("  brew uninstall awpfel");
    0
}

/// Exact source lines awpfel itself installs, for conservative uninstall
/// recognition: the install-prefix plugin path and the config-dir fallback.
/// Nothing else is ever treated as ours.
fn known_source_lines() -> Vec<String> {
    let mut known = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(prefix) = exe.parent().and_then(|p| p.parent())
    {
        known.push(source_line_for(&prefix.join("share/awpfel/awpfel.zsh")));
    }
    known.push(source_line_for(&config::config_dir().join("awpfel.zsh")));
    known
}

async fn cmd_context() -> i32 {
    if !daemon::is_running() {
        println!("awpfel daemon is not running");
        return 0;
    }

    match daemon::send_context_request().await {
        Ok(resp) => match resp {
            protocol::Response::Context(c) => {
                println!("cwd: {}", c.cwd);
                if let Some(repo) = &c.repo_type {
                    println!("repo type: {repo}");
                }
                if let Some(branch) = &c.git_branch {
                    println!("git branch: {branch}");
                }
                if !c.recent_commands.is_empty() {
                    println!("recent commands:");
                    for cmd in &c.recent_commands {
                        println!("  {cmd}");
                    }
                }
                if let Some(code) = c.last_exit_code {
                    println!("last exit code: {code}");
                }
                0
            }
            _ => {
                println!("unexpected response");
                1
            }
        },
        Err(e) => {
            eprintln!("failed to query context: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_rc(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join(".zshrc");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn shell_integration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("awpfel.zsh");
        std::fs::write(&plugin, "# stub").unwrap();
        let zshrc = write_rc(dir.path(), "export PATH=$HOME/bin:$PATH\n");

        let first = ensure_shell_integration(&zshrc, &plugin, true).unwrap();
        assert!(matches!(first, ShellOutcome::Added));
        let after_first = std::fs::read_to_string(&zshrc).unwrap();

        let second = ensure_shell_integration(&zshrc, &plugin, true).unwrap();
        assert!(matches!(second, ShellOutcome::AlreadyConfigured));
        let after_second = std::fs::read_to_string(&zshrc).unwrap();
        assert_eq!(after_first, after_second);

        let content = after_second;
        assert_eq!(content.matches(AUTOSTART_BEGIN).count(), 1);
        assert!(content.contains(&source_line_for(&plugin)));
    }

    #[test]
    fn shell_integration_preserves_unrelated_and_upstream_awen_lines() {
        // Independent upstream Awen install, lookalike filenames, and
        // commented mentions are not ours and must survive setup.
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("share").join("awpfel.zsh");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, "# stub").unwrap();
        let unrelated = "source $HOME/.local/share/awen/awen.zsh\n\
             source \"$HOME/.zsh/common.zsh\" # awpfel.zsh notes\n\
             source '/opt/other/awpfel.zsh.local'\n\
             alias ll='ls -l'\n";
        let zshrc = write_rc(dir.path(), unrelated);

        let outcome = ensure_shell_integration(&zshrc, &plugin, true).unwrap();
        assert!(matches!(outcome, ShellOutcome::Added));
        let content = std::fs::read_to_string(&zshrc).unwrap();
        for line in unrelated.lines() {
            assert!(content.contains(line), "lost unrelated line: {line}");
        }
        assert!(content.contains(&source_line_for(&plugin)));
        assert!(content.contains(AUTOSTART_BEGIN));

        // Uninstall removes only what awpfel wrote; the unrelated lines and
        // the upstream Awen install remain.
        let known = vec![source_line_for(&plugin)];
        assert!(strip_shell_integration_known(&zshrc, &known));
        let stripped = std::fs::read_to_string(&zshrc).unwrap();
        for line in unrelated.lines() {
            assert!(stripped.contains(line), "stripped unrelated line: {line}");
        }
        assert!(!stripped.contains(&source_line_for(&plugin)));
        assert!(!stripped.contains(AUTOSTART_BEGIN));
    }

    #[test]
    fn strip_preserves_content_after_unmatched_begin_marker() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "export EDITOR=vim\n{AUTOSTART_BEGIN}\nalias ll='ls -l'\nexport KEEP=1\n";
        let content = raw.replace("{AUTOSTART_BEGIN}", AUTOSTART_BEGIN);
        let zshrc = write_rc(dir.path(), &content);

        assert!(!strip_shell_integration(&zshrc));
        assert!(!has_paired_autostart_block(&content));
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), content);

        // ... and setup does not treat it as a block either: it appends a
        // fresh paired block while leaving the stray marker in place.
        let plugin = dir.path().join("awpfel.zsh");
        std::fs::write(&plugin, "# stub").unwrap();
        ensure_shell_integration(&zshrc, &plugin, true).unwrap();
        let after = std::fs::read_to_string(&zshrc).unwrap();
        assert!(after.contains("alias ll='ls -l'"));
        assert!(after.contains("export KEEP=1"));
        assert!(has_paired_autostart_block(&after));
    }

    #[test]
    fn shell_integration_quotes_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("my dir").join("awpfel.zsh");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        let line = source_line_for(&plugin);
        assert!(line.starts_with("source '") && line.ends_with('\''));
        assert!(line.contains("my dir"));

        let zshrc = write_rc(dir.path(), "");
        ensure_shell_integration(&zshrc, &plugin, true).unwrap();
        let content = std::fs::read_to_string(&zshrc).unwrap();
        assert!(content.contains(&line));
    }

    #[test]
    fn zshrc_path_respects_zdotdir() {
        let dir = tempfile::tempdir().unwrap();
        let zdotdir = dir.path().join("zdot");
        std::fs::create_dir_all(&zdotdir).unwrap();
        unsafe {
            std::env::set_var("ZDOTDIR", &zdotdir);
        }
        let path = zshrc_path();
        unsafe {
            std::env::remove_var("ZDOTDIR");
        }
        assert_eq!(path, zdotdir.join(".zshrc"));
    }

    #[test]
    fn daemon_argv_recognition_is_exact() {
        let owned = ["awpfel", "start"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert!(daemon_argv_is_awpfel(&owned));
        let prefixed = ["/opt/homebrew/bin/awpfel", "start"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert!(daemon_argv_is_awpfel(&prefixed));
        // Lookalikes must never authorize a signal.
        for foreign in [
            vec!["vim", "awpfel start notes"],
            vec!["awpfel-backup", "start"],
            vec!["awpfel"],
            vec!["awpfel", "stop"],
        ] {
            let argv = foreign.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert!(!daemon_argv_is_awpfel(&argv), "{foreign:?}");
        }
        assert!(!daemon_argv_is_awpfel(&[]));
    }
}
