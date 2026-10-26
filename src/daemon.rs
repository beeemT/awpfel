use crate::config::{self, AwpfelConfig};
use crate::context::ContextEngine;
use crate::layer::ai;
use crate::layer::failure::FailureLayer;
use crate::layer::history::HistoryLayer;
use crate::layer::risk::RiskLayer;
use crate::pipeline::{AiDecision, AiParams, AiTriggerPolicy, SuggestionPipeline};
use crate::protocol::*;

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;

struct DaemonState {
    context: ContextEngine,
    history: HistoryLayer,
    failure: FailureLayer,
    risk: RiskLayer,
    ai_provider: Option<Arc<dyn ai::AiProvider>>,
    config: AwpfelConfig,
    start_time: std::time::Instant,
    last_ai_request_at: Option<std::time::Instant>,
}

pub async fn run(config: AwpfelConfig) {
    let paths = DaemonPaths {
        socket: config::socket_path(),
        db: config::history_db_path(),
        config_dir: config::config_dir(),
    };
    write_pid();
    run_on_paths(config, &paths).await;
    cleanup_pid();
}

pub struct DaemonPaths {
    pub socket: std::path::PathBuf,
    pub db: std::path::PathBuf,
    pub config_dir: std::path::PathBuf,
}

pub async fn run_on_paths(config: AwpfelConfig, paths: &DaemonPaths) {
    run_on_paths_with_ai(config, paths, None).await;
}

pub async fn run_on_paths_with_ai(
    config: AwpfelConfig,
    paths: &DaemonPaths,
    ai_override: Option<Arc<dyn ai::AiProvider>>,
) {
    if paths.socket.exists() {
        std::fs::remove_file(&paths.socket).ok();
    }

    if let Some(parent) = paths.socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let history = match HistoryLayer::new(&paths.db) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("failed to open history db: {e}");
            eprintln!("failed to open history db: {e}");
            return;
        }
    };

    let need_history_import = history.count() == 0;
    let import_db_path = paths.db.clone();

    let mut failure = FailureLayer::new();
    failure.load_user_patterns(&paths.config_dir.join("failure_patterns.toml"));
    let risk = RiskLayer::new();

    let ai_provider = ai_override.or_else(|| ai::create_provider(&config));

    let state = Arc::new(Mutex::new(DaemonState {
        context: ContextEngine::new(&config),
        history,
        failure,
        risk,
        ai_provider,
        config: config.clone(),
        start_time: std::time::Instant::now(),
        last_ai_request_at: None,
    }));

    let listener = match UnixListener::bind(&paths.socket) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind socket: {e}");
            eprintln!("failed to bind socket at {}: {e}", paths.socket.display());
            return;
        }
    };

    tracing::info!("listening on {}", paths.socket.display());
    println!("awpfel daemon running (socket: {})", paths.socket.display());

    if need_history_import {
        tokio::task::spawn_blocking(move || {
            let histfile = config::default_zsh_histfile();
            if !histfile.exists() {
                tracing::info!(
                    "no zsh history at {}, skipping auto-import",
                    histfile.display()
                );
                return;
            }
            tracing::info!("empty history DB, importing from {}", histfile.display());
            match crate::layer::history_import::import_zsh_history(&import_db_path, &histfile) {
                Ok(r) => {
                    tracing::info!(
                        "history import: {} imported, {} sequences, {} sensitive skipped, {} empty skipped",
                        r.imported,
                        r.sequences_imported,
                        r.skipped_sensitive,
                        r.skipped_empty,
                    );
                }
                Err(e) => {
                    tracing::warn!("history import failed: {e}");
                }
            }
        });
    }

    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_clone = shutdown.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown_clone.notify_one();
    });

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, state, shutdown).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                    }
                }
            }
            _ = shutdown.notified() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    std::fs::remove_file(&paths.socket).ok();
    println!("awpfel daemon stopped");
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let response = match serde_json::from_str::<Request>(line.trim()) {
                    Ok(request) => process_request(request, &state, &shutdown).await,
                    Err(e) => Response::Error {
                        message: format!("invalid request: {e}"),
                    },
                };

                let mut resp_json = serde_json::to_string(&response).unwrap();
                resp_json.push('\n');
                if writer.write_all(resp_json.as_bytes()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

async fn process_request(
    request: Request,
    state: &Arc<Mutex<DaemonState>>,
    shutdown: &Arc<tokio::sync::Notify>,
) -> Response {
    match request {
        Request::Suggest(req) => handle_suggest(req, state).await,
        Request::NlGenerate(req) => handle_nl_generate(req, state).await,
        Request::Record(req) => handle_record(req, state).await,
        Request::Status => handle_status(state).await,
        Request::Context => handle_context(state).await,
        Request::Shutdown => {
            shutdown.notify_one();
            Response::Ok
        }
    }
}

async fn handle_suggest(req: SuggestRequest, state: &Arc<Mutex<DaemonState>>) -> Response {
    let (signals, ai_params, decision, ui_mode, history_matches) = {
        let mut st = state.lock().await;

        // Local layers produce only non-ghost UI signals (risk warning, failure
        // hint). Ghost text comes exclusively from the AI phase.
        let signals = {
            let st_ref = &mut *st;
            SuggestionPipeline::collect_local_signals(
                &st_ref.failure,
                &st_ref.risk,
                &mut st_ref.context,
                &req.input,
                &req.context,
                st_ref.config.ui.risk_detection,
            )
        };

        // History is AI input, not competing output: matching historical
        // commands go into the prompt as sanitized, bounded snippets —
        // untrusted context the model picks from, never user-facing output.
        let raw_matches = st.history.match_for_prompt(
            &req.input,
            &req.context.cwd,
            req.context.last_command.as_deref(),
            12,
        );
        let history_matches = crate::sanitize::sanitize_history_matches(&raw_matches, 12);

        let ai_params = st.ai_provider.clone().map(|provider| AiParams {
            provider,
            max_tokens: st.config.ai.max_tokens,
            timeout_ms: st.config.ai.timeout_ms,
            stderr_max_chars: st.config.context.stderr_max_chars,
            error_recovery_enabled: st.config.ai.features.error_recovery,
        });

        let policy = AiTriggerPolicy::new(&st.config.ai);
        let decision = policy.evaluate(crate::pipeline::AiEligibility {
            input: &req.input,
            signals: &signals,
            ai_available: ai_params.is_some(),
            skip_ai: req.skip_ai,
            last_ai_request_at: st.last_ai_request_at,
        });

        if decision == AiDecision::RequestNow {
            st.last_ai_request_at = Some(std::time::Instant::now());
        }

        let ui_mode = st.config.ui.mode.to_string();

        (signals, ai_params, decision, ui_mode, history_matches)
    };

    let mut response = SuggestionPipeline::execute(
        signals,
        decision,
        ai_params,
        &req.input,
        &req.context,
        &history_matches,
    )
    .await;
    response.ui_mode = Some(ui_mode);

    Response::Suggest(response)
}

async fn handle_nl_generate(req: NlGenerateRequest, state: &Arc<Mutex<DaemonState>>) -> Response {
    let (ai_provider, max_tokens, timeout_ms, stderr_max_chars, risk_enabled, nl_enabled) = {
        let state = state.lock().await;
        (
            state.ai_provider.clone(),
            state.config.ai.max_tokens,
            state.config.ai.timeout_ms,
            state.config.context.stderr_max_chars,
            state.config.ui.risk_detection,
            state.config.ai.features.nl_generation,
        )
    };

    if !nl_enabled {
        return Response::NlGenerate(NlGenerateResponse {
            command: None,
            explanation: None,
            warning: None,
        });
    }

    let Some(provider) = ai_provider else {
        return Response::NlGenerate(NlGenerateResponse {
            command: None,
            explanation: None,
            warning: None,
        });
    };

    // Oversized NL queries yield no result rather than a truncated-semantics
    // call: the shell surfaces empty/error, never a clipped-query command.
    if req.query.is_empty() || req.query.len() > ai::MAX_INPUT_BYTES {
        return Response::NlGenerate(NlGenerateResponse {
            command: None,
            explanation: None,
            warning: None,
        });
    }

    let mut ctx = req.context.clone();
    crate::sanitize::sanitize_request_context(&mut ctx, stderr_max_chars);
    let prompt = ai::build_nl_prompt(&req.query, &ctx);
    tracing::info!("NL generation started, query_len={}", req.query.len());

    let ai_start = std::time::Instant::now();
    match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        provider.complete_nl(&prompt, max_tokens),
    )
    .await
    {
        Ok(Ok(ai_response)) => {
            tracing::info!(
                "NL generation received, latency_ms={}, response_len={}",
                ai_start.elapsed().as_millis(),
                ai_response.len()
            );
            if let Some(suggestion) = ai::parse_nl_suggestion(&ai_response) {
                tracing::info!("NL suggestion accepted, len={}", suggestion.text.len());

                let warning = if risk_enabled {
                    let state = state.lock().await;
                    state.risk.check(&suggestion.text)
                } else {
                    None
                };

                return Response::NlGenerate(NlGenerateResponse {
                    command: Some(suggestion.text),
                    explanation: None,
                    warning,
                });
            }
        }
        Ok(Err(e)) => {
            tracing::info!("NL generation error: {e}");
        }
        Err(_) => {
            tracing::info!(
                "NL generation timed out, latency_ms={}",
                ai_start.elapsed().as_millis()
            );
        }
    }

    Response::NlGenerate(NlGenerateResponse {
        command: None,
        explanation: None,
        warning: None,
    })
}

async fn handle_record(req: RecordCommandRequest, state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut state = state.lock().await;

    let prev_command = state
        .context
        .session()
        .last_command()
        .map(|c| c.command.clone());

    state.context.record_command(
        req.command.clone(),
        req.exit_code,
        req.stderr.clone(),
        req.cwd.clone(),
        req.duration_ms,
    );

    if let Err(e) = state.history.record(&req.command, &req.cwd, req.exit_code) {
        tracing::warn!("failed to record command: {e}");
    }

    if let Some(prev) = prev_command
        && let Err(e) = state.history.record_sequence(&prev, &req.command)
    {
        tracing::warn!("failed to record sequence: {e}");
    }

    Response::Ok
}

async fn handle_status(state: &Arc<Mutex<DaemonState>>) -> Response {
    let state = state.lock().await;
    Response::Status(StatusResponse {
        running: true,
        pid: std::process::id(),
        uptime_secs: state.start_time.elapsed().as_secs(),
        history_count: state.history.count(),
        ai_enabled: state.config.ai.enabled,
        config: Some(StatusConfig {
            ui_mode: state.config.ui.mode.to_string(),
            ghost_text_color: state.config.ui.ghost_text_color,
            capture_stderr: state.config.context.capture_stderr,
            stderr_max_chars: state.config.context.stderr_max_chars,
            next_command_enabled: state.config.ai.enabled
                && state.config.ai.features.completion
                && state.config.ai.features.next_command,
        }),
    })
}

async fn handle_context(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut state = state.lock().await;
    let ctx = state.context.build_context_response();
    Response::Context(ctx)
}

pub fn is_running() -> bool {
    let socket_path = config::socket_path();
    if !socket_path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&socket_path).is_ok()
}

pub fn read_pid() -> Option<u32> {
    let pid_path = config::pid_path();
    std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn write_pid() {
    let pid_path = config::pid_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(pid_path, std::process::id().to_string()).ok();
}

fn cleanup_pid() {
    std::fs::remove_file(config::pid_path()).ok();
}

pub fn cleanup_socket() {
    std::fs::remove_file(config::socket_path()).ok();
    cleanup_pid();
}

pub async fn send_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    let response = send_request(&Request::Shutdown).await?;
    match response {
        Response::Ok => Ok(()),
        Response::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub async fn send_status_request() -> Result<Response, Box<dyn std::error::Error>> {
    send_request(&Request::Status).await
}

pub async fn send_context_request() -> Result<Response, Box<dyn std::error::Error>> {
    send_request(&Request::Context).await
}

async fn send_request(request: &Request) -> Result<Response, Box<dyn std::error::Error>> {
    send_request_to(&config::socket_path(), request).await
}

pub async fn send_request_to(
    socket_path: &std::path::Path,
    request: &Request,
) -> Result<Response, Box<dyn std::error::Error>> {
    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    let mut json = serde_json::to_string(request)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: Response = serde_json::from_str(line.trim())?;
    Ok(response)
}
