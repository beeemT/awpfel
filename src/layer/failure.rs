use crate::protocol::{Hint, HintKind};
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Clone)]
struct FailurePattern {
    regex: Regex,
    description_template: String,
}

#[derive(Debug, Deserialize)]
struct FailurePatternConfig {
    pattern: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct FailurePatternsFile {
    failure_patterns: Vec<FailurePatternConfig>,
}

pub struct FailureLayer {
    patterns: Vec<FailurePattern>,
}

impl Default for FailureLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl FailureLayer {
    pub fn new() -> Self {
        Self {
            patterns: builtin_patterns(),
        }
    }

    pub fn load_user_patterns(&mut self, path: &std::path::Path) {
        if !path.exists() {
            return;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<FailurePatternsFile>(&content) {
                Ok(file) => {
                    for p in file.failure_patterns {
                        if let Ok(regex) = Regex::new(&p.pattern) {
                            self.patterns.push(FailurePattern {
                                regex,
                                description_template: p.description,
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to parse failure patterns: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("failed to read failure patterns: {e}");
            }
        }
    }

    pub fn is_user_signal(exit_code: i32) -> bool {
        matches!(exit_code, 130 | 137 | 143)
    }

    /// Non-ghost failure signal: returns the hint text only. Never produces
    /// a suggestion — AI error-recovery owns ghost output.
    pub fn match_failure(&self, stderr: &str, exit_code: i32) -> Option<Hint> {
        if exit_code == 0 || Self::is_user_signal(exit_code) || stderr.is_empty() {
            return None;
        }

        for pattern in &self.patterns {
            if let Some(caps) = pattern.regex.captures(stderr) {
                let mut text = pattern.description_template.clone();
                for i in 1..caps.len() {
                    if let Some(m) = caps.get(i) {
                        let placeholder = format!("{{{i}}}");
                        text = text.replace(&placeholder, m.as_str());
                    }
                }
                return Some(Hint {
                    text,
                    kind: HintKind::FailureRecovery,
                });
            }
        }

        None
    }
}

fn builtin_patterns() -> Vec<FailurePattern> {
    let raw = vec![
        (r"cannot find crate `(\w+)`", "Looks like `{1}` is missing"),
        (r"Module not found.*'(\S+)'", "Looks like `{1}` is missing"),
        (r"No module named '(\w+)'", "Looks like `{1}` is missing"),
        (
            r"command not found: (\w+)",
            "Command `{1}` is not installed",
        ),
        (r"Permission denied", "Permission denied — try with sudo"),
        (r"port (\d+) already in use", "Port {1} is already in use"),
        (
            r"address already in use.*:(\d+)",
            "Port {1} is already in use",
        ),
        (r"EADDRINUSE.*:(\d+)", "Port {1} is already in use"),
        (
            r"Could not resolve host: (\S+)",
            "Cannot resolve host `{1}`",
        ),
        (r"fatal: not a git repository", "Not a git repository"),
        (
            r"error: failed to push some refs",
            "Remote has changes — pull first",
        ),
        (r"CONFLICT \(content\)", "Merge conflicts detected"),
        (
            r"error\[E0432\]: unresolved import `(\S+)`",
            "Unresolved import `{1}`",
        ),
        (
            r#"npm ERR! Missing script: "(\w+)""#,
            "Script `{1}` not found — check available scripts",
        ),
        (
            r"error: package `(\S+)` cannot be found",
            "Package `{1}` not found",
        ),
        (
            r"could not find `Cargo\.toml` in `(\S+)`",
            "No Cargo.toml in `{1}` — wrong directory?",
        ),
        (
            r"(\w+): illegal option -- (\w)",
            "Invalid flag `-{2}` for `{1}`",
        ),
        (
            r"(\w[\w-]*): unrecognized option '(-{1,2}\S+)'",
            "Unknown option `{2}` for `{1}`",
        ),
    ];

    raw.into_iter()
        .filter_map(|(pattern, description)| {
            Regex::new(pattern).ok().map(|regex| FailurePattern {
                regex,
                description_template: description.into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_rust_crate_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("error: cannot find crate `tokio`", 1)
            .expect("should match");
        assert_eq!(hint.text, "Looks like `tokio` is missing");
        assert_eq!(hint.kind, HintKind::FailureRecovery);
    }

    #[test]
    fn test_missing_node_module_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("Error: Module not found: Can't resolve 'axios'", 1)
            .expect("should match");
        assert_eq!(hint.text, "Looks like `axios` is missing");
    }

    #[test]
    fn test_command_not_found_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("zsh: command not found: ripgrep", 127)
            .expect("should match");
        assert_eq!(hint.text, "Command `ripgrep` is not installed");
    }

    #[test]
    fn test_permission_denied_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("bash: /usr/local/bin/thing: Permission denied", 1)
            .expect("should match");
        assert_eq!(hint.text, "Permission denied — try with sudo");
    }

    #[test]
    fn test_port_in_use_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("Error: port 3000 already in use", 1)
            .expect("should match");
        assert_eq!(hint.text, "Port 3000 is already in use");
    }

    #[test]
    fn test_no_match() {
        let layer = FailureLayer::new();
        assert!(layer.match_failure("some random error output", 1).is_none());
    }

    #[test]
    fn test_exit_code_zero() {
        let layer = FailureLayer::new();
        assert!(
            layer
                .match_failure("cannot find crate `tokio`", 0)
                .is_none()
        );
    }

    #[test]
    fn test_user_signal_exit_codes_ignored() {
        let layer = FailureLayer::new();
        for code in [130, 137, 143] {
            assert!(
                layer.match_failure("some error output", code).is_none(),
                "exit code {code} should be ignored"
            );
        }
    }

    #[test]
    fn test_missing_cargo_toml_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure(
                "error: could not find `Cargo.toml` in `/Users/saonian` or any parent directory",
                101,
            )
            .expect("should match");
        assert_eq!(
            hint.text,
            "No Cargo.toml in `/Users/saonian` — wrong directory?"
        );
    }

    #[test]
    fn test_illegal_option_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("find: illegal option -- n", 1)
            .expect("should match");
        assert_eq!(hint.text, "Invalid flag `-n` for `find`");
    }

    #[test]
    fn test_unrecognized_option_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("grep: unrecognized option '--colour'", 2)
            .expect("should match");
        assert_eq!(hint.text, "Unknown option `--colour` for `grep`");
    }

    #[test]
    fn test_git_push_rejected_hint() {
        let layer = FailureLayer::new();
        let hint = layer
            .match_failure("error: failed to push some refs to 'origin'", 1)
            .expect("should match");
        assert_eq!(hint.text, "Remote has changes — pull first");
    }
}
