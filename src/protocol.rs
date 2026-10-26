use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Suggest(SuggestRequest),
    NlGenerate(NlGenerateRequest),
    Record(RecordCommandRequest),
    Status,
    Context,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestRequest {
    pub input: String,
    pub cursor_pos: usize,
    pub context: RequestContext,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub skip_ai: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NlGenerateRequest {
    pub query: String,
    pub context: RequestContext,
    #[serde(default)]
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestContext {
    pub cwd: String,
    #[serde(default)]
    pub last_command: Option<String>,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub last_stderr: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub git_status: Option<String>,
    #[serde(default)]
    pub session_commands: Vec<String>,
    #[serde(default)]
    pub env_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordCommandRequest {
    pub command: String,
    pub exit_code: i32,
    #[serde(default)]
    pub stderr: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Suggest(SuggestResponse),
    NlGenerate(NlGenerateResponse),
    Status(StatusResponse),
    Context(ContextResponse),
    Ok,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NlGenerateResponse {
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<Warning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResponse {
    pub suggestions: Vec<Suggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<Hint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<Warning>,
    #[serde(default)]
    pub need_ai: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// The ghost-text candidate: always the FULL command the AI chose.
/// The shell renders a suffix or an explicit replacement preview from it.
pub struct Suggestion {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub text: String,
    pub kind: HintKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintKind {
    FailureRecovery,
    Explanation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusConfig {
    pub ui_mode: String,
    pub ghost_text_color: u8,
    pub capture_stderr: bool,
    pub stderr_max_chars: usize,
    #[serde(default)]
    pub next_command_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub running: bool,
    pub pid: u32,
    pub uptime_secs: u64,
    pub history_count: u64,
    pub ai_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<StatusConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextResponse {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub recent_commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggest_request_roundtrip() {
        let req = Request::Suggest(SuggestRequest {
            input: "docker run -".into(),
            cursor_pos: 12,
            context: RequestContext {
                cwd: "/home/user".into(),
                last_command: Some("docker build .".into()),
                last_exit_code: Some(0),
                last_stderr: None,
                git_branch: Some("main".into()),
                git_status: None,
                session_commands: vec!["npm run build".into()],
                env_hints: vec![],
            },
            timestamp: Some(1716100000),
            skip_ai: false,
        });
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Suggest(s) => {
                assert_eq!(s.input, "docker run -");
                assert_eq!(s.cursor_pos, 12);
                assert!(!s.skip_ai);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_suggest_request_skip_ai_roundtrip() {
        let req = Request::Suggest(SuggestRequest {
            input: "git status".into(),
            cursor_pos: 10,
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
            skip_ai: true,
        });
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"skip_ai\":true"));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Suggest(s) => assert!(s.skip_ai),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_suggest_request_skip_ai_default_false() {
        let json = r#"{"type":"suggest","input":"ls","cursor_pos":2,"context":{"cwd":"/tmp","session_commands":[],"env_hints":[]}}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        match parsed {
            Request::Suggest(s) => assert!(!s.skip_ai),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_suggest_response_roundtrip() {
        let resp = Response::Suggest(SuggestResponse {
            suggestions: vec![Suggestion {
                text: "docker run -p 3000:3000 myapp".into(),
            }],

            hint: None,
            warning: Some(Warning {
                text: "This will delete everything".into(),
            }),
            need_ai: false,
            ui_mode: None,
        });
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::Suggest(s) => {
                assert_eq!(s.suggestions.len(), 1);
                assert!(s.warning.is_some());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_record_request_roundtrip() {
        let req = Request::Record(RecordCommandRequest {
            command: "cargo build".into(),
            exit_code: 1,
            stderr: Some("cannot find crate `tokio`".into()),
            cwd: "/home/user/project".into(),
            duration_ms: Some(3200),
        });
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Record(r) => {
                assert_eq!(r.exit_code, 1);
                assert_eq!(r.command, "cargo build");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_nl_generate_request_roundtrip() {
        let req = Request::NlGenerate(NlGenerateRequest {
            query: "list files in current directory".into(),
            context: RequestContext {
                cwd: "/home/user".into(),
                last_command: None,
                last_exit_code: None,
                last_stderr: None,
                git_branch: Some("main".into()),
                git_status: None,
                session_commands: vec![],
                env_hints: vec![],
            },
            timestamp: None,
        });
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"nl_generate\""));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::NlGenerate(r) => {
                assert_eq!(r.query, "list files in current directory");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_nl_generate_response_roundtrip() {
        let resp = Response::NlGenerate(NlGenerateResponse {
            command: Some("ls -la".into()),
            explanation: None,
            warning: None,
        });
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"nl_generate\""));
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::NlGenerate(r) => {
                assert_eq!(r.command, Some("ls -la".into()));
                assert!(r.warning.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_nl_generate_response_with_warning() {
        let resp = Response::NlGenerate(NlGenerateResponse {
            command: Some("rm -rf /tmp/old".into()),
            explanation: None,
            warning: Some(Warning {
                text: "Recursive force delete".into(),
            }),
        });
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::NlGenerate(r) => {
                assert!(r.command.is_some());
                assert!(r.warning.is_some());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_nl_generate_response_no_command() {
        let resp = Response::NlGenerate(NlGenerateResponse {
            command: None,
            explanation: None,
            warning: None,
        });
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::NlGenerate(r) => {
                assert!(r.command.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }
}
