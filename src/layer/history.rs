use crate::sanitize::{SENSITIVE_COMMAND_RE, SENSITIVE_VALUE_RE};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_COMMAND_LENGTH: usize = 500;
const SUGGEST_QUERY_LIMIT: usize = 500;
// Prompt-context matching: keep every plausible historical match.
// Threshold favors recall — these strings inform the model, never the user.
const PROMPT_MIN_SCORE: u32 = 25;
const DIR_AFFINITY_SUGGEST: f64 = 1.5;
const PREFIX_BONUS: f64 = 3.0;
const FAILURE_PENALTY: f64 = 0.3;
const LAST_CMD_PENALTY: f64 = 0.1;
const SEQUENCE_BOOST_SUGGEST: f64 = 2.0;
const SEQUENCE_QUERY_LIMIT: usize = 50;

pub fn is_sensitive_command(command: &str) -> bool {
    SENSITIVE_VALUE_RE.is_match(command) || SENSITIVE_COMMAND_RE.is_match(command)
}

pub fn normalize_command(command: &str) -> String {
    let mut words = command.split_whitespace();
    let first = match words.next() {
        Some(w) => w,
        None => return String::new(),
    };
    for word in words {
        if !word.starts_with('-') {
            return format!("{first} {word}");
        }
    }
    first.to_string()
}

pub struct HistoryLayer {
    db_path: PathBuf,
}

impl HistoryLayer {
    pub fn new(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS commands (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                exit_code INTEGER NOT NULL DEFAULT 0,
                count INTEGER NOT NULL DEFAULT 1,
                UNIQUE(command, cwd)
            );
            CREATE INDEX IF NOT EXISTS idx_commands_command ON commands(command);
            CREATE INDEX IF NOT EXISTS idx_commands_cwd ON commands(cwd);
            CREATE INDEX IF NOT EXISTS idx_commands_timestamp ON commands(timestamp);
            CREATE TABLE IF NOT EXISTS command_sequences (
                prev_command TEXT NOT NULL,
                next_command TEXT NOT NULL,
                count INTEGER NOT NULL DEFAULT 1,
                UNIQUE(prev_command, next_command)
            );
            CREATE INDEX IF NOT EXISTS idx_seq_prev ON command_sequences(prev_command);",
        )?;
        Ok(Self {
            db_path: db_path.to_path_buf(),
        })
    }

    pub fn record(&self, command: &str, cwd: &str, exit_code: i32) -> Result<(), rusqlite::Error> {
        let trimmed = command.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_COMMAND_LENGTH {
            return Ok(());
        }
        if is_sensitive_command(command) {
            tracing::debug!("skipping sensitive command in history");
            return Ok(());
        }
        let conn = Connection::open(&self.db_path)?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO commands (command, cwd, timestamp, exit_code, count)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(command, cwd) DO UPDATE SET
                count = count + 1,
                timestamp = ?3,
                exit_code = ?4",
            params![command, cwd, now, exit_code],
        )?;
        Ok(())
    }

    pub fn record_sequence(
        &self,
        prev_command: &str,
        next_command: &str,
    ) -> Result<(), rusqlite::Error> {
        let prev_norm = normalize_command(prev_command);
        let next_norm = normalize_command(next_command);
        if prev_norm.is_empty() || next_norm.is_empty() || prev_norm == next_norm {
            return Ok(());
        }
        if is_sensitive_command(prev_command) || is_sensitive_command(next_command) {
            return Ok(());
        }
        let conn = Connection::open(&self.db_path)?;
        conn.execute(
            "INSERT INTO command_sequences (prev_command, next_command, count)
             VALUES (?1, ?2, 1)
             ON CONFLICT(prev_command, next_command) DO UPDATE SET
                count = count + 1",
            params![prev_norm, next_norm],
        )?;
        Ok(())
    }

    fn query_sequence_boosts(&self, prev_command: &str, weight: f64) -> HashMap<String, f64> {
        let prev_norm = normalize_command(prev_command);
        if prev_norm.is_empty() {
            return HashMap::new();
        }
        let conn = match Connection::open(&self.db_path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT next_command, count FROM command_sequences
             WHERE prev_command = ?1 ORDER BY count DESC LIMIT ?2",
        ) else {
            return HashMap::new();
        };
        let rows: Vec<(String, i64)> = match stmt
            .query_map(params![prev_norm, SEQUENCE_QUERY_LIMIT as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            }) {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => return HashMap::new(),
        };
        let max_count = rows.first().map(|(_, c)| *c).unwrap_or(1).max(1) as f64;
        rows.into_iter()
            .map(|(cmd, count)| {
                let boost = 1.0 + weight * (count as f64 / max_count);
                (cmd, boost)
            })
            .collect()
    }

    /// Ranked historical matches for AI prompt context (not ghost output).
    ///
    /// Returns full command strings ordered best-first. Flexible best-match:
    /// no hard prefix or first-token gates — every row reaches the fuzzy
    /// scorer (plus lightweight typo tolerance for transpositions nucleo
    /// alone misses). The model picks the single best full command; the UI
    /// shows non-prefix replacements explicitly.
    pub fn match_for_prompt(
        &self,
        input: &str,
        cwd: &str,
        last_command: Option<&str>,
        limit: usize,
    ) -> Vec<String> {
        if input.trim().is_empty() {
            return Vec::new();
        }

        let conn = match Connection::open(&self.db_path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let query = format!(
            "SELECT command, cwd, timestamp, count, exit_code FROM commands
             ORDER BY timestamp DESC LIMIT {SUGGEST_QUERY_LIMIT}"
        );
        let mut stmt = match conn.prepare(&query) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows: Vec<(String, String, i64, i64, i32)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i32>(4)?,
                ))
            })
            .map(|mapped| mapped.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();

        let seq_boosts = last_command
            .map(|lc| self.query_sequence_boosts(lc, SEQUENCE_BOOST_SUGGEST))
            .unwrap_or_default();

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(input, CaseMatching::Smart, Normalization::Smart);

        let now = chrono::Utc::now().timestamp();
        let mut scored: Vec<(f64, String)> = Vec::new();

        let input_lower = input.to_lowercase();
        let input_first_word = input_lower.split_whitespace().next().unwrap_or("");

        for (command, cmd_cwd, timestamp, count, exit_code) in &rows {
            // No hard gates: every row reaches the scorer. Typo tolerance
            // covers transpositions (e.g. `gti` vs `git`) that subsequence
            // matching alone can miss.
            let mut buf = Vec::new();
            let haystack = nucleo_matcher::Utf32Str::new(command, &mut buf);
            let fuzzy = pattern.score(haystack, &mut matcher).unwrap_or(0);
            let typo = typo_bonus(input, command);
            let combined = fuzzy.max(typo);
            if combined < PROMPT_MIN_SCORE {
                continue;
            }
            let age_hours = ((now - timestamp) as f64 / 3600.0).max(1.0);
            let recency_decay = 1.0 / age_hours.ln().max(1.0);
            let frequency_boost = (*count as f64).max(1.0).sqrt();
            let dir_affinity = if cmd_cwd == cwd {
                DIR_AFFINITY_SUGGEST
            } else {
                1.0
            };
            let prefix_bonus = if command.starts_with(input) {
                PREFIX_BONUS
            } else {
                1.0
            };
            let first_word_bonus = if !input_first_word.is_empty()
                && command.to_lowercase().starts_with(input_first_word)
            {
                1.5
            } else {
                1.0
            };
            let failure_penalty = if *exit_code != 0 {
                FAILURE_PENALTY
            } else {
                1.0
            };
            let last_cmd_penalty = if last_command.is_some_and(|lc| lc == command) {
                LAST_CMD_PENALTY
            } else {
                1.0
            };
            let seq_boost = seq_boosts
                .get(&normalize_command(command))
                .copied()
                .unwrap_or(1.0);

            let score = combined as f64
                * recency_decay
                * frequency_boost
                * dir_affinity
                * prefix_bonus
                * first_word_bonus
                * failure_penalty
                * last_cmd_penalty
                * seq_boost;
            scored.push((score, command.clone()));
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.dedup_by(|a, b| a.1 == b.1);

        scored
            .into_iter()
            .take(limit)
            .map(|(_, text)| text)
            .collect()
    }

    pub fn count(&self) -> u64 {
        let conn = match Connection::open(&self.db_path) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))
            .unwrap_or(0)
    }
}

// Small typo-tolerance bonus for transpositions and single edits in the
// command's first token (e.g. input `gti status` vs recorded `git status`).
// Returns a nucleo-scale score so it composes with fuzzy results directly.
fn typo_bonus(input: &str, command: &str) -> u32 {
    let input_first = input.split_whitespace().next().unwrap_or("").to_lowercase();
    let cmd_first = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if input_first.is_empty() || cmd_first.is_empty() {
        return 0;
    }
    if input_first == cmd_first {
        return 60;
    }
    let distance = strsim::levenshtein(&input_first, &cmd_first) as u32;
    if distance == 1 {
        55
    } else if distance == 2 && input_first.len() >= 3 {
        45
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, HistoryLayer) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("history.db");
        let layer = HistoryLayer::new(&db_path).unwrap();
        (dir, layer)
    }

    #[test]
    fn test_record_and_count() {
        let (_dir, layer) = setup();
        layer.record("ls -la", "/tmp", 0).unwrap();
        layer.record("pwd", "/tmp", 0).unwrap();
        assert_eq!(layer.count(), 2);
    }

    #[test]
    fn test_record_dedup() {
        let (_dir, layer) = setup();
        layer.record("ls -la", "/tmp", 0).unwrap();
        layer.record("ls -la", "/tmp", 0).unwrap();
        assert_eq!(layer.count(), 1);
    }

    #[test]
    fn test_match_for_prompt_prefix() {
        let (_dir, layer) = setup();
        layer.record("docker build .", "/app", 0).unwrap();
        layer
            .record("docker run -p 3000:3000 app", "/app", 0)
            .unwrap();
        layer.record("npm install", "/app", 0).unwrap();

        let results = layer.match_for_prompt("docker", "/app", None, 5);
        assert!(!results.is_empty());
        assert!(results.iter().all(|s| s.contains("docker")));
    }

    #[test]
    fn test_match_for_prompt_empty_input() {
        let (_dir, layer) = setup();
        layer.record("ls", "/tmp", 0).unwrap();
        let results = layer.match_for_prompt("", "/tmp", None, 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_match_for_prompt_typo_transposition() {
        let (_dir, layer) = setup();
        // Transposed first token: `gti status` must still surface `git status`.
        layer.record("git status", "/app", 0).unwrap();
        layer.record("npm install", "/app", 0).unwrap();

        let results = layer.match_for_prompt("gti status", "/app", None, 5);
        assert!(
            results.iter().any(|s| s.contains("git status")),
            "typo input should still match, got: {results:?}"
        );
    }

    #[test]
    fn test_match_for_prompt_midword_flag() {
        let (_dir, layer) = setup();
        layer
            .record("kubectl get pods --all-namespaces", "/app", 0)
            .unwrap();
        layer.record("kubectl get svc", "/app", 0).unwrap();

        let results = layer.match_for_prompt("kubectl --all", "/app", None, 5);
        assert!(
            results.iter().any(|s| s.contains("--all-namespaces")),
            "midword flag input should match flag-bearing command, got: {results:?}"
        );
    }

    #[test]
    fn test_match_for_prompt_directory_affinity() {
        let (_dir, layer) = setup();
        layer.record("make build", "/project-a", 0).unwrap();
        layer.record("make test", "/project-b", 0).unwrap();

        let results = layer.match_for_prompt("make", "/project-a", None, 5);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_is_sensitive_command_docker_login() {
        assert!(is_sensitive_command(
            "docker login registry.io -u user -p secret123"
        ));
    }

    #[test]
    fn test_is_sensitive_command_export_secret() {
        assert!(is_sensitive_command(
            "export API_KEY=sk-abcdefghijklmnopqrstuvwxyz12345"
        ));
        assert!(is_sensitive_command("export SECRET_TOKEN=abc123"));
    }

    #[test]
    fn test_is_sensitive_command_safe() {
        assert!(!is_sensitive_command("cargo build --release"));
        assert!(!is_sensitive_command("git status"));
        assert!(!is_sensitive_command("ls -la"));
        assert!(!is_sensitive_command("npm install express"));
    }

    #[test]
    fn test_record_skips_sensitive() {
        let (_dir, layer) = setup();
        layer
            .record("docker login registry.io -u user -p secret", "/tmp", 0)
            .unwrap();
        assert_eq!(layer.count(), 0);

        layer.record("cargo build", "/tmp", 0).unwrap();
        assert_eq!(layer.count(), 1);
    }

    #[test]
    fn test_match_for_prompt_typo_first_word_only_best_match() {
        let (_dir, layer) = setup();
        layer.record("git add .", "/app", 0).unwrap();
        layer.record("git commit -m 'test'", "/app", 0).unwrap();

        let results = layer.match_for_prompt("gti", "/app", None, 10);
        assert!(
            results.iter().any(|s| s.starts_with("git")),
            "transposed first token should still surface git commands, got: {results:?}"
        );
    }

    #[test]
    fn test_match_for_prompt_flag_fragment_best_match() {
        let (_dir, layer) = setup();
        layer
            .record("claude --dangerously-skip-permissions", "/app", 0)
            .unwrap();
        layer.record("claude --print config", "/app", 0).unwrap();

        // Flexible best-match: both candidates reach the scorer; the closer
        // flag fragment ranks first but nothing is hard-filtered.
        let results = layer.match_for_prompt("claude --prin", "/app", None, 10);
        assert!(!results.is_empty());
        assert_eq!(
            results[0], "claude --print config",
            "closest flag fragment should rank first, got: {results:?}"
        );
    }

    #[test]
    fn test_match_for_prompt_short_flag_not_filtered() {
        let (_dir, layer) = setup();
        layer
            .record("claude --dangerously-skip-permissions", "/app", 0)
            .unwrap();

        let results = layer.match_for_prompt("claude --", "/app", None, 10);
        assert!(
            !results.is_empty(),
            "short flag prefix (--) should not filter"
        );
    }

    #[test]
    fn test_match_for_prompt_first_word_best_match() {
        let (_dir, layer) = setup();
        layer.record("git add .", "/app", 0).unwrap();
        layer.record("git commit -m 'test'", "/app", 0).unwrap();
        layer
            .record("pkill -f daemon; sleep 1; source init", "/app", 0)
            .unwrap();
        layer.record("echo git add something", "/app", 0).unwrap();

        let results = layer.match_for_prompt("git a", "/app", None, 10);
        assert!(
            results.iter().any(|s| s.starts_with("git")),
            "best match should surface a git command, got: {results:?}"
        );
    }

    #[test]
    fn test_normalize_command() {
        assert_eq!(normalize_command("git add ."), "git add");
        assert_eq!(normalize_command("git commit -m 'fix'"), "git commit");
        assert_eq!(normalize_command("cargo test --lib"), "cargo test");
        assert_eq!(normalize_command("ls -la"), "ls");
        assert_eq!(
            normalize_command("docker run -p 3000:3000 app"),
            "docker run"
        );
        assert_eq!(normalize_command("npm install express"), "npm install");
        assert_eq!(normalize_command("pwd"), "pwd");
        assert_eq!(normalize_command(""), "");
        assert_eq!(normalize_command("  "), "");
    }

    #[test]
    fn test_record_sequence_basic() {
        let (_dir, layer) = setup();
        layer
            .record_sequence("git add .", "git commit -m 'test'")
            .unwrap();

        let boosts = layer.query_sequence_boosts("git add .", SEQUENCE_BOOST_SUGGEST);
        assert!(boosts.contains_key("git commit"));
    }

    #[test]
    fn test_record_sequence_count_increment() {
        let (_dir, layer) = setup();
        for _ in 0..5 {
            layer
                .record_sequence("git add .", "git commit -m 'x'")
                .unwrap();
        }
        layer.record_sequence("git add .", "git status").unwrap();

        let boosts = layer.query_sequence_boosts("git add", SEQUENCE_BOOST_SUGGEST);
        let commit_boost = boosts.get("git commit").copied().unwrap_or(1.0);
        let status_boost = boosts.get("git status").copied().unwrap_or(1.0);
        assert!(
            commit_boost > status_boost,
            "higher count should get higher boost"
        );
    }

    #[test]
    fn test_record_sequence_sensitive_skipped() {
        let (_dir, layer) = setup();
        layer
            .record_sequence("export API_KEY=sk-abc123", "curl http://example.com")
            .unwrap();

        let boosts =
            layer.query_sequence_boosts("export API_KEY=sk-abc123", SEQUENCE_BOOST_SUGGEST);
        assert!(boosts.is_empty());
    }

    #[test]
    fn test_record_sequence_same_command_skipped() {
        let (_dir, layer) = setup();
        layer.record_sequence("ls -la", "ls -l").unwrap();

        let boosts = layer.query_sequence_boosts("ls", SEQUENCE_BOOST_SUGGEST);
        assert!(
            boosts.is_empty(),
            "same normalized command should be skipped"
        );
    }

    #[test]
    fn test_match_for_prompt_quoted_commit_message() {
        let (_dir, layer) = setup();
        // Quoted prose inside a real command is retrievable history, not prose.
        layer
            .record("git commit -m 'removes the stale cache'", "/app", 0)
            .unwrap();
        layer.record("npm install", "/app", 0).unwrap();

        let results = layer.match_for_prompt("git commit", "/app", None, 10);
        assert!(
            results
                .iter()
                .any(|s| s.contains("removes the stale cache")),
            "quoted command should be retrievable, got: {results:?}"
        );
    }

    #[test]
    fn test_match_for_prompt_with_sequence_boost() {
        let (_dir, layer) = setup();
        layer.record("git commit -m 'test'", "/app", 0).unwrap();
        layer.record("git checkout main", "/app", 0).unwrap();

        for _ in 0..5 {
            layer
                .record_sequence("git add .", "git commit -m 'x'")
                .unwrap();
        }

        let results = layer.match_for_prompt("git", "/app", Some("git add ."), 10);
        let commit_pos = results.iter().position(|s| s.contains("commit"));
        let checkout_pos = results.iter().position(|s| s.contains("checkout"));
        if let (Some(c), Some(co)) = (commit_pos, checkout_pos) {
            assert!(
                c < co,
                "sequenced git commit should rank above git checkout"
            );
        }
    }
}
