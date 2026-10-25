use std::sync::Arc;
use std::time::Instant;

use crate::config::{AiConfig, AiFeaturesConfig};
use crate::context::ContextEngine;
use crate::layer::ai::{self, AiProvider};
use crate::layer::failure::FailureLayer;
use crate::layer::risk::RiskLayer;
use crate::protocol::{Hint, RequestContext, SuggestResponse, Suggestion, Warning};

pub struct LocalSignals {
    pub warning: Option<Warning>,
    pub hint: Option<Hint>,
    pub has_failure_context: bool,
    pub local_failure_matched: bool,
}

pub struct AiParams {
    pub provider: Arc<dyn AiProvider>,
    pub max_tokens: u32,
    pub timeout_ms: u64,
    pub stderr_max_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiDecision {
    RequestNow,
    NeedAiOnly,
    Skip,
}

pub struct AiTriggerPolicy {
    debounce_ms: u64,
    features: AiFeaturesConfig,
}

/// Eligibility snapshot for one suggest call: the current input plus the
/// already-collected local signals (warning/hint/failure context), provider
/// availability, and debounce state. The policy reads this single snapshot;
/// callers never pass the underlying fields positionally.
pub struct AiEligibility<'a> {
    pub input: &'a str,
    pub signals: &'a LocalSignals,
    pub ai_available: bool,
    pub skip_ai: bool,
    pub last_ai_request_at: Option<Instant>,
}

impl AiTriggerPolicy {
    pub fn new(config: &AiConfig) -> Self {
        Self {
            debounce_ms: config.debounce_ms,
            features: config.features.clone(),
        }
    }

    /// Decide whether the async AI phase is eligible.
    ///
    /// AI is the sole producer of ghost text, so local match strength never
    /// gates the decision: any eligible nonempty debounced input triggers a
    /// call, with relevant history supplied as prompt context instead of
    /// competing output.
    pub fn evaluate(&self, e: AiEligibility<'_>) -> AiDecision {
        if !self.ai_is_useful(e.input, e.signals) {
            return AiDecision::Skip;
        }
        if !e.ai_available {
            return AiDecision::Skip;
        }
        if e.skip_ai {
            return AiDecision::NeedAiOnly;
        }
        if let Some(last_at) = e.last_ai_request_at
            && (last_at.elapsed().as_millis() as u64) < self.debounce_ms
        {
            return AiDecision::NeedAiOnly;
        }
        AiDecision::RequestNow
    }

    fn ai_is_useful(&self, input: &str, signals: &LocalSignals) -> bool {
        // Risk warnings suppress AI even though the warning itself is kept.
        if signals.warning.is_some() {
            return false;
        }
        // Error recovery supplements empty-input failure context only through
        // AI. Normal nonempty completion must not be hijacked by the
        // error-recovery switch: when the user is typing, completion decides.
        if signals.has_failure_context && input.is_empty() {
            if signals.local_failure_matched {
                return false;
            }
            return self.features.error_recovery;
        }
        if !self.features.completion {
            return false;
        }
        if input.len() < 2 || input.len() > crate::layer::ai::MAX_INPUT_BYTES {
            // Oversized input: no result rather than a truncated-semantics call.
            return false;
        }
        if input.chars().any(|c| {
            c == '\0'
                || c == '\u{1b}'
                || c == '\r'
                || c == '\u{7f}'
                || c.is_control() && c != '\n' && c != '\t'
        }) {
            return false;
        }
        true
    }
}

enum AiTask<'a> {
    ErrorRecovery {
        context: &'a RequestContext,
        history_matches: &'a [String],
    },
    Completion {
        input: &'a str,
        context: &'a RequestContext,
        history_matches: &'a [String],
    },
}

pub struct SuggestionPipeline;

impl SuggestionPipeline {
    /// Collect local non-ghost signals only: risk warning + failure hint.
    ///
    /// History is AI prompt context, never ghost output, and the AI phase
    /// is the sole producer of ghost text.
    pub fn collect_local_signals(
        failure: &FailureLayer,
        risk: &RiskLayer,
        context: &mut ContextEngine,
        input: &str,
        req_context: &RequestContext,
        risk_enabled: bool,
    ) -> LocalSignals {
        context.update_cwd(req_context.cwd.clone());

        let mut hint = None;
        let local_failure_matched = if let Some(exit_code) = req_context.last_exit_code
            && exit_code != 0
            && let Some(stderr) = &req_context.last_stderr
            && let Some(fail_hint) = failure.match_failure(stderr, exit_code)
        {
            hint = Some(fail_hint);
            true
        } else {
            false
        };

        let has_failure_context = req_context
            .last_exit_code
            .is_some_and(|c| c != 0 && !FailureLayer::is_user_signal(c))
            && req_context.last_stderr.is_some();

        let warning = if risk_enabled && !input.is_empty() {
            risk.check(input)
        } else {
            None
        };

        LocalSignals {
            warning,
            hint,
            has_failure_context,
            local_failure_matched,
        }
    }

    pub async fn execute(
        signals: LocalSignals,
        decision: AiDecision,
        ai_params: Option<AiParams>,
        input: &str,
        context: &RequestContext,
        history_matches: &[String],
    ) -> SuggestResponse {
        let mut response = SuggestResponse {
            suggestions: Vec::new(),
            hint: signals.hint,
            warning: signals.warning,
            need_ai: matches!(decision, AiDecision::RequestNow | AiDecision::NeedAiOnly),
            ui_mode: None,
        };

        if decision == AiDecision::RequestNow
            && let Some(ref params) = ai_params
        {
            // Failure error-recovery and normal completion share the AI path.
            // Optional error-recovery never hijacks normal completion: the
            // task splits on empty-input failure context only; when the user
            // is typing, completion decides. Local matches never gate AI.
            let task = if signals.has_failure_context && input.is_empty() {
                AiTask::ErrorRecovery {
                    context,
                    history_matches,
                }
            } else {
                AiTask::Completion {
                    input,
                    context,
                    history_matches,
                }
            };

            if let Some(suggestion) = Self::execute_ai(task, params).await {
                response.suggestions.push(suggestion);
            }
        }

        response
    }

    async fn execute_ai(task: AiTask<'_>, params: &AiParams) -> Option<Suggestion> {
        // Honor the per-shell context exactly as supplied: an empty
        // session_commands list stays empty. No server-global recent-command
        // fallback — cross-shell leakage is worse than a thinner prompt.
        let mut ctx = match &task {
            AiTask::ErrorRecovery { context, .. } => (*context).clone(),
            AiTask::Completion { context, .. } => (*context).clone(),
        };
        crate::sanitize::sanitize_request_context(&mut ctx, params.stderr_max_chars);

        let (prompt, label) = match &task {
            AiTask::ErrorRecovery {
                history_matches, ..
            } => (
                ai::build_error_recovery_prompt(&ctx, history_matches),
                "error recovery",
            ),
            AiTask::Completion {
                input,
                history_matches,
                ..
            } => (ai::build_prompt(input, &ctx, history_matches), "completion"),
        };

        tracing::info!("AI {label} started");
        let start = Instant::now();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(params.timeout_ms),
            match &task {
                AiTask::ErrorRecovery { .. } => {
                    params.provider.complete_nl(&prompt, params.max_tokens)
                }
                AiTask::Completion { .. } => params.provider.complete(&prompt, params.max_tokens),
            },
        )
        .await;

        match result {
            Ok(Ok(ai_response)) => {
                tracing::info!(
                    "AI {label} received, latency_ms={}",
                    start.elapsed().as_millis()
                );
                match &task {
                    AiTask::ErrorRecovery { .. } => ai::parse_nl_suggestion(&ai_response),
                    AiTask::Completion { input, .. } => {
                        let suggestion = ai::parse_ai_suggestion(input, &ai_response);
                        if suggestion.is_some() {
                            tracing::info!("AI suggestion accepted");
                        } else {
                            tracing::info!("AI suggestion rejected by parser");
                        }
                        suggestion
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::info!("AI {label} error: {e}");
                // No fabricated fallback: failure/timeout yields no ghost.
                None
            }
            Err(_) => {
                tracing::info!(
                    "AI {label} timed out, latency_ms={}",
                    start.elapsed().as_millis()
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> AiTriggerPolicy {
        AiTriggerPolicy {
            debounce_ms: 300,
            features: AiFeaturesConfig {
                error_recovery: true,
                completion: true,
                nl_generation: true,
            },
        }
    }

    struct Case<'a> {
        input: &'a str,
        has_failure_context: bool,
        local_failure_matched: bool,
        warning: Option<Warning>,
        ai_available: bool,
        skip_ai: bool,
        last_ai_request_at: Option<Instant>,
    }

    fn decide(p: &AiTriggerPolicy, c: Case<'_>) -> AiDecision {
        p.evaluate(AiEligibility {
            input: c.input,
            signals: &LocalSignals {
                warning: c.warning,
                hint: None,
                has_failure_context: c.has_failure_context,
                local_failure_matched: c.local_failure_matched,
            },
            ai_available: c.ai_available,
            skip_ai: c.skip_ai,
            last_ai_request_at: c.last_ai_request_at,
        })
    }

    fn base(input: &str) -> Case<'_> {
        Case {
            input,
            has_failure_context: false,
            local_failure_matched: false,
            warning: None,
            ai_available: true,
            skip_ai: false,
            last_ai_request_at: None,
        }
    }

    fn warn() -> Option<Warning> {
        Some(Warning {
            text: "dangerous".into(),
        })
    }

    // Nonempty eligible input always requests AI: local match strength never
    // gates the decision anymore (history is prompt context, not output).

    #[test]
    fn warning_blocks_ai() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "git ch",
                    warning: warn(),
                    ..base("")
                },
            ),
            AiDecision::Skip
        );
    }

    #[test]
    fn oversized_and_control_input_skips_ai() {
        let p = policy();
        // Oversized input yields no result rather than a changed-semantics call.
        let big = "x".repeat(crate::layer::ai::MAX_INPUT_BYTES + 1);
        assert_eq!(
            decide(
                &p,
                Case {
                    input: &big,
                    ..base("")
                }
            ),
            AiDecision::Skip
        );
        // Terminal controls before BUFFER yield no ghost.
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "ls \x1b[0m",
                    ..base("")
                }
            ),
            AiDecision::Skip
        );
    }

    #[test]
    fn error_recovery_triggers_ai_without_local_match() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    has_failure_context: true,
                    ..base("")
                },
            ),
            AiDecision::RequestNow
        );
        // Nonempty input with failure context is normal completion, never
        // hijacked by the error-recovery switch.
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "gi",
                    has_failure_context: true,
                    ..base("")
                },
            ),
            AiDecision::RequestNow
        );
    }

    #[test]
    fn error_recovery_local_matched_skips_only_empty_input() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    has_failure_context: true,
                    local_failure_matched: true,
                    ..base("")
                },
            ),
            AiDecision::Skip
        );
        // A locally matched hint does not suppress typing-time completion.
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "gi",
                    has_failure_context: true,
                    local_failure_matched: true,
                    ..base("")
                },
            ),
            AiDecision::RequestNow
        );
    }

    #[test]
    fn short_input_no_failure_skips() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "g",
                    ..base("")
                }
            ),
            AiDecision::Skip
        );
        assert_eq!(decide(&p, base("")), AiDecision::Skip);
    }

    #[test]
    fn eligible_input_always_requests_ai() {
        let p = policy();
        // Strong local history would previously have suppressed AI; it no
        // longer does — history informs the prompt instead.
        for input in ["git ch", "docker run", "myapp", "myapp d"] {
            assert_eq!(
                decide(&p, Case { input, ..base("") }),
                AiDecision::RequestNow,
                "input {input:?} should request AI regardless of local matches"
            );
        }
    }

    #[test]
    fn warning_blocks_request() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "rm -rf /",
                    warning: warn(),
                    ..base("")
                },
            ),
            AiDecision::Skip
        );
    }

    #[test]
    fn debounce_returns_need_ai_only() {
        let p = policy();
        let recent = Instant::now();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "docker run",
                    last_ai_request_at: Some(recent),
                    ..base("")
                },
            ),
            AiDecision::NeedAiOnly
        );
    }

    #[test]
    fn happy_path_requests_ai() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "docker run",
                    ..base("")
                }
            ),
            AiDecision::RequestNow
        );
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "myapp",
                    ..base("")
                }
            ),
            AiDecision::RequestNow
        );
    }

    #[test]
    fn error_recovery_bypasses_input_check() {
        let p = policy();
        // Empty input + failure context is the error-recovery path.
        assert_eq!(
            decide(
                &p,
                Case {
                    has_failure_context: true,
                    ..base("")
                },
            ),
            AiDecision::RequestNow
        );
        // Nonempty failure context goes through completion, so a disabled
        // error-recovery switch never blocks typing-time AI.
        let no_recovery = AiTriggerPolicy {
            debounce_ms: 300,
            features: AiFeaturesConfig {
                error_recovery: false,
                completion: true,
                nl_generation: true,
            },
        };
        assert_eq!(
            decide(
                &no_recovery,
                Case {
                    input: "gi",
                    has_failure_context: true,
                    ..base("")
                },
            ),
            AiDecision::RequestNow
        );
        assert_eq!(
            decide(
                &no_recovery,
                Case {
                    has_failure_context: true,
                    ..base("")
                },
            ),
            AiDecision::Skip
        );
    }

    #[test]
    fn skip_ai_returns_need_ai_only() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "docker run",
                    skip_ai: true,
                    ..base("")
                },
            ),
            AiDecision::NeedAiOnly
        );
    }

    #[test]
    fn no_ai_provider_skips() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "docker run",
                    ai_available: false,
                    ..base("")
                },
            ),
            AiDecision::Skip
        );
    }

    #[test]
    fn error_recovery_with_skip_ai_returns_need_ai_only() {
        let p = policy();
        assert_eq!(
            decide(
                &p,
                Case {
                    has_failure_context: true,
                    skip_ai: true,
                    ..base("")
                },
            ),
            AiDecision::NeedAiOnly
        );
    }

    #[test]
    fn completion_disabled_skips_nonempty_input() {
        let p = AiTriggerPolicy {
            debounce_ms: 300,
            features: AiFeaturesConfig {
                error_recovery: true,
                completion: false,
                nl_generation: true,
            },
        };
        assert_eq!(
            decide(
                &p,
                Case {
                    input: "docker run",
                    ..base("")
                }
            ),
            AiDecision::Skip
        );
    }
}
