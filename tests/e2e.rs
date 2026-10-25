use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use awpfel::config::AwpfelConfig;
use awpfel::daemon::{self, DaemonPaths};
use awpfel::layer::ai::{AiError, AiProvider};
use awpfel::protocol::*;

fn test_paths(dir: &std::path::Path) -> DaemonPaths {
    DaemonPaths {
        socket: dir.join("awpfel-test.sock"),
        db: dir.join("history-test.db"),
        config_dir: dir.join("config"),
    }
}

fn test_config() -> AwpfelConfig {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = false;
    config.context.repo_detect = false;
    config.context.git_context = false;
    config
}

struct TestDaemon {
    socket_path: PathBuf,
    _dir: tempfile::TempDir,
}

struct SlowMockProvider;

#[async_trait::async_trait]
impl AiProvider for SlowMockProvider {
    async fn complete(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok("mock-slow-result".into())
    }

    async fn complete_nl(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok("echo hello".into())
    }
}

struct FastMockProvider {
    call_count: std::sync::atomic::AtomicU32,
}

impl FastMockProvider {
    fn new() -> Self {
        Self {
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn calls(&self) -> u32 {
        self.call_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl AiProvider for FastMockProvider {
    async fn complete(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("pods --all-namespaces".into())
    }

    async fn complete_nl(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("kubectl get pods --all-namespaces".into())
    }
}

struct CapturingProvider {
    response: String,
    last_prompt: std::sync::Mutex<Option<String>>,
    call_count: std::sync::atomic::AtomicU32,
}

impl CapturingProvider {
    fn new(response: &str) -> Self {
        Self {
            response: response.into(),
            last_prompt: std::sync::Mutex::new(None),
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn prompt(&self) -> Option<String> {
        self.last_prompt.lock().unwrap().clone()
    }

    fn record(&self, prompt: &str) {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.last_prompt.lock().unwrap() = Some(prompt.to_string());
    }
}

#[async_trait::async_trait]
impl AiProvider for CapturingProvider {
    async fn complete(&self, prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        self.record(prompt);
        Ok(self.response.clone())
    }

    async fn complete_nl(&self, prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        self.record(prompt);
        Ok(self.response.clone())
    }
}

// Model with a small context window: errors when the output reserve would
// overflow the supported budget. Proves the NL route honors the configured
// small max_tokens instead of forcing a large floor.
struct SmallBudgetProvider {
    context_window: u32,
}

#[async_trait::async_trait]
impl AiProvider for SmallBudgetProvider {
    async fn complete(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        Err(AiError::RequestFailed("unsupported route".into()))
    }

    async fn complete_nl(&self, prompt: &str, max_tokens: u32) -> Result<String, AiError> {
        let prompt_tokens = prompt.len() as u32 / 4;
        if prompt_tokens + max_tokens > self.context_window {
            return Err(AiError::RequestFailed(format!(
                "context overflow: {prompt_tokens} + {max_tokens} > {}",
                self.context_window
            )));
        }
        Ok("kubectl get pods --all-namespaces".into())
    }
}

struct FailingProvider;

#[async_trait::async_trait]
impl AiProvider for FailingProvider {
    async fn complete(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        Err(AiError::RequestFailed("injected failure".into()))
    }

    async fn complete_nl(&self, _prompt: &str, _max_tokens: u32) -> Result<String, AiError> {
        Err(AiError::RequestFailed("injected failure".into()))
    }
}

impl TestDaemon {
    async fn start() -> Self {
        Self::start_with_config_and_ai(test_config(), None).await
    }

    async fn start_with_ai(config: AwpfelConfig, provider: Arc<dyn AiProvider>) -> Self {
        Self::start_with_config_and_ai(config, Some(provider)).await
    }

    async fn start_with_config_and_ai(
        config: AwpfelConfig,
        ai_override: Option<Arc<dyn AiProvider>>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let socket_path = paths.socket.clone();

        tokio::spawn(async move {
            daemon::run_on_paths_with_ai(config, &paths, ai_override).await;
        });

        for _ in 0..50 {
            let ready =
                socket_path.exists() && tokio::net::UnixStream::connect(&socket_path).await.is_ok();
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            socket_path.exists(),
            "daemon socket not created within timeout"
        );

        Self {
            socket_path,
            _dir: dir,
        }
    }

    async fn send(&self, request: &Request) -> Response {
        daemon::send_request_to(&self.socket_path, request)
            .await
            .expect("failed to send request")
    }

    async fn shutdown(self) {
        let resp = self.send(&Request::Shutdown).await;
        assert!(matches!(resp, Response::Ok));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ============================================================
// Daemon 生命周期测试
// ============================================================

#[tokio::test]
async fn test_daemon_start_and_status() {
    let daemon = TestDaemon::start().await;

    let resp = daemon.send(&Request::Status).await;
    match resp {
        Response::Status(s) => {
            assert!(s.running);
            assert!(!s.ai_enabled);
            assert_eq!(s.history_count, 0);
            let cfg = s.config.expect("status should include config");
            assert_eq!(cfg.ui_mode, "minimal");
        }
        other => panic!("expected Status response, got: {other:?}"),
    }

    let socket_path = daemon.socket_path.clone();
    daemon.shutdown().await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !socket_path.exists(),
        "socket file should be cleaned up after shutdown"
    );
}

#[tokio::test]
async fn test_daemon_multiple_connections() {
    let daemon = TestDaemon::start().await;

    let (r1, r2, r3) = tokio::join!(
        daemon.send(&Request::Status),
        daemon.send(&Request::Status),
        daemon.send(&Request::Status),
    );

    assert!(matches!(r1, Response::Status(_)));
    assert!(matches!(r2, Response::Status(_)));
    assert!(matches!(r3, Response::Status(_)));

    daemon.shutdown().await;
}

// ============================================================
// Record + Context 追踪测试
// ============================================================

#[tokio::test]
async fn test_record_and_context() {
    let daemon = TestDaemon::start().await;

    let record_req = Request::Record(RecordCommandRequest {
        command: "cargo build".into(),
        exit_code: 0,
        stderr: None,
        cwd: "/home/user/project".into(),
        duration_ms: Some(1500),
    });
    let resp = daemon.send(&record_req).await;
    assert!(matches!(resp, Response::Ok));

    let record_req2 = Request::Record(RecordCommandRequest {
        command: "cargo test".into(),
        exit_code: 1,
        stderr: Some("test result: FAILED. 1 passed; 2 failed".into()),
        cwd: "/home/user/project".into(),
        duration_ms: Some(3200),
    });
    daemon.send(&record_req2).await;

    let resp = daemon.send(&Request::Context).await;
    match resp {
        Response::Context(c) => {
            assert_eq!(c.cwd, "/home/user/project");
            assert!(c.recent_commands.contains(&"cargo build".to_string()));
            assert!(c.recent_commands.contains(&"cargo test".to_string()));
            assert_eq!(c.last_exit_code, Some(1));
        }
        other => panic!("expected Context response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_context_cwd_tracking() {
    let daemon = TestDaemon::start().await;

    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "ls".into(),
            exit_code: 0,
            stderr: None,
            cwd: "/tmp/dir1".into(),
            duration_ms: None,
        }))
        .await;

    let resp = daemon.send(&Request::Context).await;
    match &resp {
        Response::Context(c) => assert_eq!(c.cwd, "/tmp/dir1"),
        other => panic!("expected Context, got: {other:?}"),
    }

    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "pwd".into(),
            exit_code: 0,
            stderr: None,
            cwd: "/tmp/dir2".into(),
            duration_ms: None,
        }))
        .await;

    let resp = daemon.send(&Request::Context).await;
    match resp {
        Response::Context(c) => assert_eq!(c.cwd, "/tmp/dir2"),
        other => panic!("expected Context, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// Suggest 管道 E2E 测试
// ============================================================

#[tokio::test]
async fn test_suggest_risk_warning() {
    let daemon = TestDaemon::start().await;

    let req = Request::Suggest(SuggestRequest {
        input: "rm -rf /".into(),
        cursor_pos: 8,
        context: RequestContext {
            cwd: "/home/user".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                s.warning.is_some(),
                "should get risk warning for 'rm -rf /'"
            );
            assert!(
                s.suggestions.is_empty(),
                "risk warning input produces no ghost"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_suggest_no_warning_for_safe_command() {
    let daemon = TestDaemon::start().await;

    let req = Request::Suggest(SuggestRequest {
        input: "ls -la".into(),
        cursor_pos: 6,
        context: RequestContext {
            cwd: "/home/user".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                s.warning.is_none(),
                "should NOT get warning for safe command 'ls -la'"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// 完整流程测试：Record → Suggest → Context
// ============================================================

#[tokio::test]
async fn test_full_session_flow() {
    let daemon = TestDaemon::start().await;

    // 1. 初始状态
    let resp = daemon.send(&Request::Status).await;
    match &resp {
        Response::Status(s) => {
            assert!(s.running);
            assert_eq!(s.history_count, 0);
        }
        other => panic!("expected Status, got: {other:?}"),
    }

    // 2. 模拟用户执行命令
    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "npm install".into(),
            exit_code: 0,
            stderr: None,
            cwd: "/home/user/webapp".into(),
            duration_ms: Some(12000),
        }))
        .await;

    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "npm run dev".into(),
            exit_code: 0,
            stderr: None,
            cwd: "/home/user/webapp".into(),
            duration_ms: Some(500),
        }))
        .await;

    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "npm test".into(),
            exit_code: 1,
            stderr: Some(r#"npm ERR! Missing script: "test""#.into()),
            cwd: "/home/user/webapp".into(),
            duration_ms: Some(200),
        }))
        .await;

    // 3. history count 增长
    let resp = daemon.send(&Request::Status).await;
    match &resp {
        Response::Status(s) => assert!(
            s.history_count >= 3,
            "should have at least 3 history entries"
        ),
        other => panic!("expected Status, got: {other:?}"),
    }

    // 4. Context 反映最后的失败
    let resp = daemon.send(&Request::Context).await;
    match &resp {
        Response::Context(c) => {
            assert_eq!(c.cwd, "/home/user/webapp");
            assert_eq!(c.last_exit_code, Some(1));
        }
        other => panic!("expected Context, got: {other:?}"),
    }

    // 5. Suggest：AI-disabled daemon 不产生 ghost 建议（AI 是唯一的 ghost 来源）
    let req = Request::Suggest(SuggestRequest {
        input: "".into(),
        cursor_pos: 0,
        context: RequestContext {
            cwd: "/home/user/webapp".into(),
            last_command: Some("npm test".into()),
            last_exit_code: Some(1),
            last_stderr: Some(r#"npm ERR! Missing script: "test""#.into()),
            git_branch: None,
            git_status: None,
            session_commands: vec![
                "npm install".into(),
                "npm run dev".into(),
                "npm test".into(),
            ],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });
    let resp = daemon.send(&req).await;
    match &resp {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "AI-disabled daemon should produce no ghost suggestions, got: {:?}",
                s.suggestions.iter().map(|sg| &sg.text).collect::<Vec<_>>()
            );
        }
        other => panic!("expected Suggest, got: {other:?}"),
    }

    // 6. Suggest：输入 'npm ' 同样无建议（AI disabled → 无 ghost）
    let req = Request::Suggest(SuggestRequest {
        input: "npm ".into(),
        cursor_pos: 4,
        context: RequestContext {
            cwd: "/home/user/webapp".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });
    let resp = daemon.send(&req).await;
    match &resp {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "AI-disabled daemon should produce no suggestions for 'npm ', got: {:?}",
                s.suggestions.iter().map(|sg| &sg.text).collect::<Vec<_>>()
            );
        }
        other => panic!("expected Suggest, got: {other:?}"),
    }

    // 7. 风险检测仍然在工作
    let req = Request::Suggest(SuggestRequest {
        input: "git push --force".into(),
        cursor_pos: 16,
        context: RequestContext {
            cwd: "/home/user/webapp".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: Some("main".into()),
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });
    let resp = daemon.send(&req).await;
    match &resp {
        Response::Suggest(s) => {
            assert!(s.warning.is_some(), "should warn about 'git push --force'");
        }
        other => panic!("expected Suggest, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// 协议错误处理测试
// ============================================================

#[tokio::test]
async fn test_invalid_json_request() {
    let daemon = TestDaemon::start().await;

    let stream = tokio::net::UnixStream::connect(&daemon.socket_path)
        .await
        .unwrap();
    let (reader, mut writer) = stream.into_split();

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    writer.write_all(b"{\"bad json\n").await.unwrap();

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    match resp {
        Response::Error { message } => {
            assert!(
                message.contains("invalid"),
                "error should mention invalid request, got: {message}"
            );
        }
        other => panic!("expected Error response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// Security tests
// ============================================================

#[tokio::test]
async fn test_record_sensitive_command_skipped() {
    let daemon = TestDaemon::start().await;

    let initial_status = daemon.send(&Request::Status).await;
    let initial_count = match initial_status {
        Response::Status(s) => s.history_count,
        _ => panic!("expected Status"),
    };

    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "docker login registry.io -u user -p secret123".into(),
            exit_code: 0,
            stderr: None,
            cwd: "/tmp".into(),
            duration_ms: None,
        }))
        .await;

    let status = daemon.send(&Request::Status).await;
    match status {
        Response::Status(s) => {
            assert_eq!(
                s.history_count, initial_count,
                "sensitive command should not be recorded"
            );
        }
        other => panic!("expected Status, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_suggest_with_sensitive_context() {
    let daemon = TestDaemon::start().await;

    let response = daemon
        .send(&Request::Suggest(SuggestRequest {
            input: "git status".into(),
            cursor_pos: 10,
            context: RequestContext {
                cwd: "/tmp".into(),
                last_command: Some(
                    "curl -H 'Authorization: Bearer sk-abcdefghijklmnopqrstuvwxyz12345'".into(),
                ),
                last_exit_code: Some(1),
                last_stderr: Some("error: token sk-abcdefghijklmnopqrstuvwxyz12345 invalid".into()),
                git_branch: Some("main".into()),
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
            skip_ai: false,
        }))
        .await;

    match response {
        Response::Suggest(_) => {}
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_risk_warning_with_suggestions_coexist() {
    let daemon = TestDaemon::start().await;

    daemon
        .send(&Request::Record(RecordCommandRequest {
            command: "rm -rf ./build".into(),
            exit_code: 0,
            stderr: None,
            cwd: "/project".into(),
            duration_ms: None,
        }))
        .await;

    let response = daemon
        .send(&Request::Suggest(SuggestRequest {
            input: "rm -rf /".into(),
            cursor_pos: 8,
            context: RequestContext {
                cwd: "/project".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
            skip_ai: false,
        }))
        .await;

    match response {
        Response::Suggest(s) => {
            assert!(s.warning.is_some(), "risk warning should be present");
        }
        other => panic!("expected Suggest, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// AI merge + debounce E2E tests
// ============================================================

#[tokio::test]
async fn test_ai_suggestion_merged_into_response() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider: Arc<dyn AiProvider> = Arc::new(FastMockProvider::new());
    let daemon = TestDaemon::start_with_ai(config, provider).await;

    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert_eq!(
                s.suggestions.len(),
                1,
                "AI ghost is exactly one full command"
            );
            assert!(
                s.suggestions[0].text.contains("pods"),
                "AI suggestion should contain mock completion, got: {}",
                s.suggestions[0].text
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_ai_debounce_skips_second_request() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 5000;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let make_req = |input: &str| {
        Request::Suggest(SuggestRequest {
            input: input.into(),
            cursor_pos: input.len(),
            context: RequestContext {
                cwd: "/home/user/project".into(),
                last_command: None,
                last_exit_code: Some(0),
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
            skip_ai: false,
        })
    };

    let resp1 = daemon.send(&make_req("myapp deploy")).await;
    match &resp1 {
        Response::Suggest(s) => {
            assert_eq!(s.suggestions.len(), 1, "first request should trigger AI");
        }
        other => panic!("expected Suggest, got: {other:?}"),
    }
    assert_eq!(
        provider.calls(),
        1,
        "AI should be called once for first request"
    );

    let resp2 = daemon.send(&make_req("myapp rollback")).await;
    match &resp2 {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "second request within debounce window should NOT have AI suggestion"
            );
        }
        other => panic!("expected Suggest, got: {other:?}"),
    }
    assert_eq!(
        provider.calls(),
        1,
        "AI should NOT be called again within debounce window"
    );

    daemon.shutdown().await;
}

// ============================================================
// skip_ai E2E tests
// ============================================================

#[tokio::test]
async fn test_skip_ai_returns_local_only() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: true,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "skip_ai=true should return no suggestions when AI is the sole producer"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }
    assert_eq!(
        provider.calls(),
        0,
        "AI provider should never be called when skip_ai=true"
    );

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_skip_ai_fast_response_with_slow_provider() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider: Arc<dyn AiProvider> = Arc::new(SlowMockProvider);
    let daemon = TestDaemon::start_with_ai(config, provider).await;

    let req = Request::Suggest(SuggestRequest {
        input: "git ch".into(),
        cursor_pos: 6,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: true,
    });

    let start = std::time::Instant::now();
    let resp = daemon.send(&req).await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(100),
        "skip_ai=true should return in <100ms even with slow AI provider (took {:?})",
        elapsed
    );

    match resp {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "skip_ai=true should return no suggestions"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_skip_ai_false_still_calls_ai() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert_eq!(
                s.suggestions.len(),
                1,
                "AI ghost is exactly one full command"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }
    assert_eq!(
        provider.calls(),
        1,
        "AI provider should be called when skip_ai=false"
    );

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_skip_ai_false_risk_warning_still_skips_ai() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let req = Request::Suggest(SuggestRequest {
        input: "rm -rf /".into(),
        cursor_pos: 8,
        context: RequestContext {
            cwd: "/home/user".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(s.warning.is_some(), "should still detect risk warning");
            assert!(
                s.suggestions.is_empty(),
                "AI should be skipped when risk warning is present"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }
    assert_eq!(
        provider.calls(),
        0,
        "AI should not be called when risk warning is present"
    );

    daemon.shutdown().await;
}

// ============================================================
// AI-as-fallback E2E tests
// ============================================================

#[tokio::test]
async fn test_ai_called_when_local_insufficient() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy --region".into(),
        cursor_pos: 21,
        context: RequestContext {
            cwd: "/tmp".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                provider.calls() > 0,
                "AI should be called for eligible input"
            );
            assert_eq!(
                s.suggestions.len(),
                1,
                "AI ghost is exactly one full command"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_ai_error_recovery_when_failure_unmatched() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let req = Request::Suggest(SuggestRequest {
        input: "".into(),
        cursor_pos: 0,
        context: RequestContext {
            cwd: "/tmp".into(),
            last_command: Some("myapp migrate".into()),
            last_exit_code: Some(1),
            last_stderr: Some("database connection timeout after 30s".into()),
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                provider.calls() > 0,
                "AI should be called for error recovery when local patterns miss"
            );
            assert_eq!(
                s.suggestions.len(),
                1,
                "AI ghost is exactly one full command"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_need_ai_signal_in_phase1_response() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    // Unknown command: AI is the sole producer → need_ai is true
    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/tmp".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: true,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                s.need_ai,
                "need_ai should be true when AI is the sole producer"
            );
            assert_eq!(provider.calls(), 0, "skip_ai=true should not call AI");
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    // Known command: local never satisfies ghost demand → need_ai is still true
    let req2 = Request::Suggest(SuggestRequest {
        input: "git ".into(),
        cursor_pos: 4,
        context: RequestContext {
            cwd: "/tmp".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: true,
    });

    let resp2 = daemon.send(&req2).await;
    match resp2 {
        Response::Suggest(s) => {
            assert!(
                s.need_ai,
                "need_ai should be true even for known commands: AI is the sole producer of ghost suggestions"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// AI-first ghost completion E2E tests
// ============================================================

#[tokio::test]
async fn test_ai_called_even_with_exact_history_match() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(FastMockProvider::new());
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    for _ in 0..5 {
        daemon
            .send(&Request::Record(RecordCommandRequest {
                command: "git checkout main".into(),
                exit_code: 0,
                stderr: None,
                cwd: "/home/user/project".into(),
                duration_ms: None,
            }))
            .await;
    }

    let req = Request::Suggest(SuggestRequest {
        input: "git checkout main".into(),
        cursor_pos: 17,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert_eq!(
                provider.calls(),
                1,
                "AI must be called even when history contains an exact match"
            );
            assert_eq!(
                s.suggestions.len(),
                1,
                "AI ghost is exactly one full command"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_history_context_in_ai_prompt() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(CapturingProvider::new("kubectl get pods --all-namespaces"));
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    for cmd in [
        "myapp deploy alpha-one",
        "myapp deploy alpha-two",
        "myapp deploy alpha-three",
    ] {
        daemon
            .send(&Request::Record(RecordCommandRequest {
                command: cmd.into(),
                exit_code: 0,
                stderr: None,
                cwd: "/home/user/project".into(),
                duration_ms: None,
            }))
            .await;
    }

    // NONEMPTY and UNRELATED per-shell session list: no fallback can mask a
    // missed DB retrieval. The recorded DB command must reach the prompt.
    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec!["ls -la /tmp".into(), "echo unrelated-shell-context".into()],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(_) => {}
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    let prompt = provider
        .prompt()
        .expect("AI provider should have been called");
    // Assert the retrieved DB command itself reaches the model prompt, even
    // though the per-shell session list is nonempty and unrelated.
    assert!(
        prompt.contains("myapp deploy alpha-three"),
        "AI prompt should include the DB-matched history command"
    );

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_completion_full_command_nonprefix() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider = Arc::new(CapturingProvider::new("kubectl get pods --all-namespaces"));
    let provider_clone: Arc<dyn AiProvider> = provider.clone();
    let daemon = TestDaemon::start_with_ai(config, provider_clone).await;

    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert_eq!(
                s.suggestions.len(),
                1,
                "AI ghost is exactly one full command"
            );
            assert!(
                s.suggestions[0].text.contains("kubectl"),
                "AI ghost should carry the full command, got: {}",
                s.suggestions[0].text
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_ai_failure_returns_no_ghost() {
    let mut config = AwpfelConfig::default();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.debounce_ms = 0;
    config.context.repo_detect = false;
    config.context.git_context = false;

    let provider: Arc<dyn AiProvider> = Arc::new(FailingProvider);
    let daemon = TestDaemon::start_with_ai(config, provider).await;

    let req = Request::Suggest(SuggestRequest {
        input: "myapp deploy".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "AI failure should yield no ghost suggestions"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_ai_disabled_returns_no_ghost() {
    let daemon = TestDaemon::start().await;

    let req = Request::Suggest(SuggestRequest {
        input: "git checkout".into(),
        cursor_pos: 12,
        context: RequestContext {
            cwd: "/home/user/project".into(),
            last_command: None,
            last_exit_code: Some(0),
            last_stderr: None,
            git_branch: None,
            git_status: None,
            session_commands: vec![],
            env_hints: vec![],
        },
        timestamp: None,
        skip_ai: false,
    });

    let resp = daemon.send(&req).await;
    match resp {
        Response::Suggest(s) => {
            assert!(
                s.suggestions.is_empty(),
                "AI-disabled daemon should produce no ghost suggestions"
            );
        }
        other => panic!("expected Suggest response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

// ============================================================
// NL Generate protocol tests
// ============================================================

#[tokio::test]
async fn test_nl_generate_no_ai_returns_none() {
    let daemon = TestDaemon::start().await;

    let resp = daemon
        .send(&Request::NlGenerate(NlGenerateRequest {
            query: "list files in current directory".into(),
            context: RequestContext {
                cwd: "/tmp".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
        }))
        .await;

    match resp {
        Response::NlGenerate(r) => {
            assert!(
                r.command.is_none(),
                "no AI provider, command should be None"
            );
            assert!(r.warning.is_none());
        }
        other => panic!("expected NlGenerate response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_nl_generate_empty_query_returns_none() {
    let daemon = TestDaemon::start().await;

    let resp = daemon
        .send(&Request::NlGenerate(NlGenerateRequest {
            query: "".into(),
            context: RequestContext {
                cwd: "/tmp".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
        }))
        .await;

    match resp {
        Response::NlGenerate(r) => {
            assert!(r.command.is_none(), "empty query should return None");
        }
        other => panic!("expected NlGenerate response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_nl_generate_with_ai_provider() {
    let mut config = test_config();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.features.nl_generation = true;
    config.ai.timeout_ms = 5000;

    let provider = Arc::new(FastMockProvider::new());
    let daemon = TestDaemon::start_with_ai(config, provider.clone()).await;

    let resp = daemon
        .send(&Request::NlGenerate(NlGenerateRequest {
            query: "get all pods across namespaces".into(),
            context: RequestContext {
                cwd: "/tmp".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
        }))
        .await;

    match resp {
        Response::NlGenerate(r) => {
            assert!(r.command.is_some(), "AI provider should produce a command");
            assert!(provider.calls() > 0, "AI provider should have been called");
        }
        other => panic!("expected NlGenerate response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_nl_respects_configured_budget() {
    // Model consumer bug: a forced large output reserve overflowed small
    // context windows. With a 256-token configured budget inside a 4096
    // window, the route must succeed; a max(4096) floor would fail.
    let mut config = test_config();
    config.ai.enabled = true;
    config.ai.max_tokens = 256;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.features.nl_generation = true;
    config.ai.timeout_ms = 5000;

    let provider: Arc<dyn AiProvider> = Arc::new(SmallBudgetProvider {
        context_window: 4096,
    });
    let daemon = TestDaemon::start_with_ai(config, provider).await;

    let resp = daemon
        .send(&Request::NlGenerate(NlGenerateRequest {
            query: "get all pods across namespaces".into(),
            context: RequestContext {
                cwd: "/tmp".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
        }))
        .await;

    match resp {
        Response::NlGenerate(r) => {
            assert_eq!(
                r.command,
                Some("kubectl get pods --all-namespaces".to_string()),
                "small-budget model should produce a command with the configured reserve"
            );
        }
        other => panic!("expected NlGenerate response, got: {other:?}"),
    }

    daemon.shutdown().await;
}

#[tokio::test]
async fn test_nl_failure_returns_none() {
    let mut config = test_config();
    config.ai.enabled = true;
    config.ai.features.completion = true;
    config.ai.features.error_recovery = true;
    config.ai.features.nl_generation = true;
    config.ai.timeout_ms = 5000;

    let provider: Arc<dyn AiProvider> = Arc::new(FailingProvider);
    let daemon = TestDaemon::start_with_ai(config, provider).await;

    let resp = daemon
        .send(&Request::NlGenerate(NlGenerateRequest {
            query: "get all pods across namespaces".into(),
            context: RequestContext {
                cwd: "/tmp".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: None,
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
        }))
        .await;

    match resp {
        Response::NlGenerate(r) => {
            assert!(
                r.command.is_none(),
                "failed AI should yield no command, got: {:?}",
                r.command
            );
        }
        other => panic!("expected NlGenerate response, got: {other:?}"),
    }

    daemon.shutdown().await;
}
