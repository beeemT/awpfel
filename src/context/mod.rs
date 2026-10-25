pub mod git;
pub mod repo;
pub mod session;

use crate::config::AwpfelConfig;
use crate::protocol::{ContextResponse, RequestContext};
use crate::sanitize;
use repo::RepoInfo;
use session::{CommandRecord, SessionContext};
use std::path::PathBuf;

pub struct ContextEngine {
    session: SessionContext,
    cached_repo: Option<(PathBuf, Option<RepoInfo>)>,
    config: AwpfelConfig,
}

impl ContextEngine {
    pub fn new(config: &AwpfelConfig) -> Self {
        Self {
            session: SessionContext::new(config.context.session_history_size),
            cached_repo: None,
            config: config.clone(),
        }
    }

    pub fn record_command(
        &mut self,
        command: String,
        exit_code: i32,
        stderr: Option<String>,
        cwd: String,
        duration_ms: Option<u64>,
    ) {
        let sanitized_stderr =
            stderr.map(|s| sanitize::sanitize_stderr(&s, self.config.context.stderr_max_chars));
        self.session.record(CommandRecord {
            command,
            exit_code,
            stderr: sanitized_stderr,
            cwd,
            timestamp: chrono::Utc::now().timestamp(),
            duration_ms,
        });
    }

    pub fn update_cwd(&mut self, cwd: String) {
        self.session.current_cwd = cwd;
    }

    pub fn get_repo_info(&mut self) -> Option<&RepoInfo> {
        let cwd = PathBuf::from(&self.session.current_cwd);
        if !self.config.context.repo_detect {
            return None;
        }

        let needs_refresh = match &self.cached_repo {
            Some((cached_path, _)) => *cached_path != cwd,
            None => true,
        };

        if needs_refresh {
            let info = repo::detect_repo_type(&cwd);
            self.cached_repo = Some((cwd, info));
        }

        self.cached_repo
            .as_ref()
            .and_then(|(_, info)| info.as_ref())
    }

    pub fn get_git_context(&self) -> Option<git::GitContext> {
        if !self.config.context.git_context {
            return None;
        }
        let cwd = std::path::Path::new(&self.session.current_cwd);
        git::detect_git_context(cwd)
    }

    pub fn build_request_context(&mut self) -> RequestContext {
        let git_ctx = self.get_git_context();

        let repo_type_str = self.get_repo_info().map(|r| r.repo_type.to_string());

        let env_hints: Vec<String> = std::env::vars()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "NODE_ENV" | "RUST_LOG" | "GOPATH" | "VIRTUAL_ENV" | "CONDA_DEFAULT_ENV"
                )
            })
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        let mut session_commands = self.session.recent_command_strings();
        if let Some(repo) = repo_type_str {
            session_commands.push(format!("[repo:{repo}]"));
        }

        RequestContext {
            cwd: self.session.current_cwd.clone(),
            last_command: self.session.last_command().map(|c| c.command.clone()),
            last_exit_code: self.session.last_exit_code(),
            last_stderr: self.session.last_stderr().map(String::from),
            git_branch: git_ctx.as_ref().and_then(|g| g.branch.clone()),
            git_status: git_ctx.as_ref().and_then(|g| g.status_string()),
            session_commands,
            env_hints: sanitize::sanitize_env_hints(&env_hints),
        }
    }

    pub fn build_context_response(&mut self) -> ContextResponse {
        let repo_type_str = self.get_repo_info().map(|r| r.repo_type.to_string());
        let git_ctx = self.get_git_context();

        ContextResponse {
            cwd: self.session.current_cwd.clone(),
            repo_type: repo_type_str,
            git_branch: git_ctx.as_ref().and_then(|g| g.branch.clone()),
            recent_commands: self
                .session
                .recent_commands(10)
                .into_iter()
                .map(String::from)
                .collect(),
            last_exit_code: self.session.last_exit_code(),
        }
    }

    pub fn session(&self) -> &SessionContext {
        &self.session
    }
}
