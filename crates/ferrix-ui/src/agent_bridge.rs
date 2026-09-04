//! The agent bridge: drive the RUNNING app, visibly (issue: agent compat).
//!
//! An agent writes plain-text commands to `<workbook>.fxagent`; the app —
//! when the user has switched the bridge ON — executes them one at a time
//! through the SAME paths keyboard input takes: selection moves on screen,
//! edits go through validation and the undo history, the status line narrates
//! every step (which also feeds the Selection panel's activity log). Nothing
//! is silent, nothing bypasses the UI: the user watches the agent work, and
//! the Agent window shows every executed line VERBATIM — formulas included.
//!
//! ## Protocol (one command per line, `#` comments ignored)
//!
//! ```text
//! select E1:E200          # move the visible selection
//! put G1 Bearing          # type into a cell (validation + undo + status)
//! put H1 =SUMIFS(...)     # formulas too — the rest of the line is the text
//! get M1:N6               # append the displayed values to <file>.out as TSV
//! chart N1:N6 bar O       # chart a range: kind, optional x-label column
//! svg C:/path/chart.svg   # export the current chart, no dialog
//! ```
//!
//! ## Harness-agnostic launching
//!
//! The Agent window can START an agent, not just listen for one. The launch
//! command is a user-configured template (prefs `agent_command`), split into
//! argv BEFORE placeholder substitution, so a prompt can never smuggle extra
//! arguments or shell syntax — there is no shell involved at all:
//!
//! ```text
//! hermes run --profile rust-dev {prompt}     # Hermes
//! claude -p {prompt}                         # Claude Code
//! codex exec {prompt}                        # Codex — any CLI works
//! ```
//!
//! `{prompt}` expands to the user's request plus a short protocol briefing
//! (where the command file is, what the verbs are). `{fxagent}` and
//! `{workbook}` expand to the respective paths. The same values are also
//! exported as `FERRIX_AGENT_FILE` / `FERRIX_WORKBOOK` / `FERRIX_PROMPT`
//! environment variables for CLIs that prefer them.
//!
//! ## Safety posture
//!
//! OFF by default, every session. The toggle is a deliberate user action, the
//! watched path is derived from the open workbook (never arbitrary), commands
//! only ever do what the keyboard could, and the file is consumed from the
//! offset at attach time — a stale command file cannot replay into a fresh
//! session.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One parsed command.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCmd {
    Select(String),
    Put {
        cell: String,
        text: String,
    },
    Get(String),
    Chart {
        range: String,
        kind: Option<String>,
        x_col: Option<String>,
    },
    /// Custom chart text: `label title=Profit by Region; y=Profit ($)` —
    /// key=value pairs separated by `;`. Keys: title, x, y, series.
    Label {
        title: Option<String>,
        x: Option<String>,
        y: Option<String>,
        series: Option<String>,
    },
    /// Add extra Y columns as series: `series I K` (column letters or A1
    /// cells). Replaces the current extra-series list.
    Series(Vec<String>),
    Svg(PathBuf),
}

/// One executed command, kept for the Agent window's log: the line VERBATIM
/// (so `put H1 =SUMIFS(...)` shows the whole formula), when it ran relative
/// to attach, and what came of it.
pub struct LogEntry {
    /// Seconds since the bridge attached, for a compact `t+12.4s` stamp.
    pub at_secs: f64,
    /// The command line exactly as the agent wrote it.
    pub raw: String,
    /// The status-line outcome of executing it.
    pub outcome: String,
}

/// The log keeps the most recent entries only — an agent looping forever
/// must not grow memory without bound (the scale invariant applies to
/// tooling too).
pub const MAX_LOG: usize = 500;

/// Parse one line. `None` for blanks, comments, and anything malformed —
/// malformed lines are surfaced by the caller so typos do not vanish.
pub fn parse_line(line: &str) -> Result<Option<AgentCmd>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    match verb.to_ascii_lowercase().as_str() {
        "select" if !rest.is_empty() => Ok(Some(AgentCmd::Select(rest.to_string()))),
        "put" => {
            let (cell, text) = rest
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("put needs a cell and text: {line:?}"))?;
            Ok(Some(AgentCmd::Put {
                cell: cell.to_string(),
                text: text.trim().to_string(),
            }))
        }
        "get" if !rest.is_empty() => Ok(Some(AgentCmd::Get(rest.to_string()))),
        "chart" if !rest.is_empty() => {
            let mut parts = rest.split_whitespace();
            let range = parts.next().unwrap_or_default().to_string();
            let kind = parts.next().map(|s| s.to_ascii_lowercase());
            let x_col = parts.next().map(|s| s.to_string());
            Ok(Some(AgentCmd::Chart { range, kind, x_col }))
        }
        "svg" if !rest.is_empty() => Ok(Some(AgentCmd::Svg(PathBuf::from(rest)))),
        "label" if !rest.is_empty() => {
            let mut title = None;
            let mut x = None;
            let mut y = None;
            let mut series = None;
            for part in rest.split(';') {
                let Some((k, v)) = part.split_once('=') else {
                    return Err(format!(
                        "label expects key=value pairs separated by ';': {line:?}"
                    ));
                };
                let v = v.trim().to_string();
                match k.trim().to_ascii_lowercase().as_str() {
                    "title" => title = Some(v),
                    "x" => x = Some(v),
                    "y" => y = Some(v),
                    "series" => series = Some(v),
                    other => return Err(format!("unknown label key {other:?} in {line:?}")),
                }
            }
            Ok(Some(AgentCmd::Label {
                title,
                x,
                y,
                series,
            }))
        }
        "series" => Ok(Some(AgentCmd::Series(
            rest.split_whitespace().map(str::to_string).collect(),
        ))),
        _ => Err(format!("unknown agent command: {line:?}")),
    }
}

/// Split a launch template into argv tokens: whitespace separates, double
/// quotes group. NO shell is involved — this is the whole grammar, which is
/// the point: what you see in the template is exactly what gets exec'd.
pub fn split_template(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in template.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Substitute placeholders in ONE already-split argv token. Because the
/// template was tokenised first, a `{prompt}` containing spaces, quotes, or
/// `;` stays a single argument — prompt content cannot become extra
/// arguments, let alone shell syntax.
pub fn substitute(token: &str, prompt: &str, fxagent: &Path, workbook: &Path) -> String {
    token
        .replace("{prompt}", prompt)
        .replace("{fxagent}", &fxagent.display().to_string())
        .replace("{workbook}", &workbook.display().to_string())
}

/// The protocol briefing appended to every launched prompt, so ANY agent CLI
/// — Hermes, Claude Code, Codex, a shell script — knows how to answer
/// without Ferrix-specific tooling.
pub fn protocol_briefing(fxagent: &Path, workbook: &Path) -> String {
    format!(
        "\n\n---\nYou are driving a LIVE Ferrix spreadsheet (workbook: {wb}).\n\
         To act, APPEND lines to the command file: {fx}\n\
         One command per line; the app executes them visibly, in order:\n\
         select <A1[:B2]>  — move the on-screen selection\n\
         put <cell> <text> — type into a cell (formulas start with =)\n\
         get <A1:B2>       — the app appends the displayed values, as TSV, to {fx}.out\n\
         chart <range> [bar|line|histogram|scatter] [label-col-letter]\n\
         series <col letters…>  — overlay extra value columns as series (e.g. series I K)\n\
         label <k=v;…>     — custom chart text: label title=Profit by Region; x=Region; y=Profit; series=Profit\n\
         svg <path>        — export the current chart as SVG\n\
         Lines starting with # are comments. Append, never rewrite the file.\n\
         After a get, wait for {fx}.out to grow before reading it.",
        wb = workbook.display(),
        fx = fxagent.display(),
    )
}

/// Watcher state: which file, how much of it has been consumed, the pacing
/// that keeps execution watchable, and the verbatim log the Agent window
/// shows.
pub struct AgentBridge {
    pub enabled: bool,
    path: Option<PathBuf>,
    /// Bytes of the command file already consumed. Starts at the file's
    /// length at attach time, so pre-existing content never replays.
    offset: u64,
    /// Partial trailing line (no newline yet) carried between polls.
    partial: String,
    queue: std::collections::VecDeque<(String, AgentCmd)>,
    last_poll: Option<Instant>,
    last_exec: Option<Instant>,
    /// Minimum time between executed commands — the watchability throttle.
    throttle: Duration,
    /// Commands executed since attach, for the status line.
    pub executed: usize,
    /// When the bridge attached — log stamps are relative to this.
    attached_at: Option<Instant>,
    /// The verbatim execution log (most recent last, capped at [`MAX_LOG`]).
    pub log: Vec<LogEntry>,
}

impl Default for AgentBridge {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            offset: 0,
            partial: String::new(),
            queue: std::collections::VecDeque::new(),
            last_poll: None,
            last_exec: None,
            throttle: Duration::from_millis(80),
            executed: 0,
            attached_at: None,
            log: Vec::new(),
        }
    }
}

impl AgentBridge {
    /// Attach to a command file and enable. Consumption starts at the file's
    /// CURRENT length: only commands appended after this moment run.
    pub fn attach(&mut self, path: PathBuf, throttle: Duration) {
        self.offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.path = Some(path);
        self.enabled = true;
        self.throttle = throttle;
        self.partial.clear();
        self.queue.clear();
        self.executed = 0;
        self.last_poll = None;
        self.last_exec = None;
        self.attached_at = Some(Instant::now());
        self.log.clear();
    }

    /// Detach and stop. The log is KEPT so the user can still read what the
    /// agent did after switching the bridge off.
    pub fn detach(&mut self) {
        self.enabled = false;
        self.path = None;
        self.queue.clear();
        self.partial.clear();
    }

    /// The watched command file, while attached.
    pub fn watch_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The `.out` file `get` responses append to.
    pub fn out_path(&self) -> Option<PathBuf> {
        self.path.as_ref().map(|p| {
            let mut o = p.as_os_str().to_owned();
            o.push(".out");
            PathBuf::from(o)
        })
    }

    /// Record an executed command and its outcome, verbatim, for the Agent
    /// window. Oldest entries fall off past [`MAX_LOG`].
    pub fn push_log(&mut self, raw: String, outcome: String) {
        let at_secs = self
            .attached_at
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        self.log.push(LogEntry {
            at_secs,
            raw,
            outcome,
        });
        if self.log.len() > MAX_LOG {
            let drop = self.log.len() - MAX_LOG;
            self.log.drain(..drop);
        }
    }

    /// Poll the file for appended bytes (rate-limited), parse complete lines,
    /// and return the next command — with the line it came from, verbatim —
    /// if the throttle allows one. Errors are returned so the app can surface
    /// bad lines in the status bar.
    pub fn tick(&mut self) -> Result<Option<(String, AgentCmd)>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let now = Instant::now();
        let poll_due = self
            .last_poll
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(150));
        if poll_due {
            self.last_poll = Some(now);
            if let Some(path) = &self.path {
                if let Ok(meta) = std::fs::metadata(path) {
                    // A truncated/rewritten file restarts from zero — the
                    // agent deleted and began a new script.
                    if meta.len() < self.offset {
                        self.offset = 0;
                        self.partial.clear();
                    }
                    if meta.len() > self.offset {
                        if let Ok(bytes) = read_from(path, self.offset) {
                            self.offset += bytes.len() as u64;
                            let text = String::from_utf8_lossy(&bytes);
                            let combined = format!("{}{}", self.partial, text);
                            self.partial.clear();
                            let complete_up_to = combined.rfind('\n').map(|i| i + 1);
                            let (complete, rest) = match complete_up_to {
                                Some(i) => combined.split_at(i),
                                None => ("", combined.as_str()),
                            };
                            self.partial = rest.to_string();
                            for line in complete.lines() {
                                match parse_line(line) {
                                    Ok(Some(cmd)) => {
                                        self.queue.push_back((line.trim().to_string(), cmd))
                                    }
                                    Ok(None) => {}
                                    Err(e) => return Err(e),
                                }
                            }
                        }
                    }
                }
            }
        }
        let exec_due = self
            .last_exec
            .is_none_or(|t| now.duration_since(t) >= self.throttle);
        if exec_due {
            if let Some(item) = self.queue.pop_front() {
                self.last_exec = Some(now);
                self.executed += 1;
                return Ok(Some(item));
            }
        }
        Ok(None)
    }
}

/// Read a file's bytes from `offset` to EOF.
fn read_from(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_into_commands_and_bad_lines_say_so() {
        assert_eq!(
            parse_line("select A1:B2").unwrap(),
            Some(AgentCmd::Select("A1:B2".into()))
        );
        assert_eq!(
            parse_line("put G1 =SUM(A1:A5)").unwrap(),
            Some(AgentCmd::Put {
                cell: "G1".into(),
                text: "=SUM(A1:A5)".into()
            })
        );
        assert_eq!(
            parse_line("chart N1:N6 bar O").unwrap(),
            Some(AgentCmd::Chart {
                range: "N1:N6".into(),
                kind: Some("bar".into()),
                x_col: Some("O".into())
            })
        );
        assert_eq!(parse_line("# comment").unwrap(), None);
        assert_eq!(parse_line("").unwrap(), None);
        assert!(parse_line("frobnicate A1").is_err());
        assert!(parse_line("put G1").is_err());
    }

    #[test]
    fn the_bridge_consumes_only_appended_commands_in_order() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("fxagent-{}.txt", std::process::id()));
        std::fs::write(&path, "put A1 stale\n").unwrap();

        let mut b = AgentBridge::default();
        b.attach(path.clone(), Duration::ZERO);
        // The pre-existing line must NOT replay.
        assert_eq!(b.tick().unwrap().map(|(_, c)| c), None);

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "select B2").unwrap();
        writeln!(f, "put B2 42").unwrap();
        f.flush().unwrap();

        // Poll interval: force by resetting the poll clock.
        b.last_poll = None;
        assert_eq!(
            b.tick().unwrap(),
            Some(("select B2".to_string(), AgentCmd::Select("B2".into())))
        );
        b.last_poll = None;
        let (raw, cmd) = b.tick().unwrap().expect("second command");
        assert_eq!(raw, "put B2 42", "the raw line is carried verbatim");
        assert_eq!(
            cmd,
            AgentCmd::Put {
                cell: "B2".into(),
                text: "42".into()
            }
        );
        b.last_poll = None;
        assert_eq!(b.tick().unwrap().map(|(_, c)| c), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_log_keeps_verbatim_lines_and_stays_bounded() {
        let mut b = AgentBridge::default();
        b.attach(
            std::env::temp_dir().join("fxagent-log-test"),
            Duration::ZERO,
        );
        b.push_log(
            "put H1 =SUMIFS(D1:D200, C1:C200, G1, B1:B200, H8)".into(),
            "H1 updated".into(),
        );
        assert!(
            b.log[0].raw.contains("=SUMIFS(D1:D200"),
            "the log shows the full formula, not a summary"
        );
        for i in 0..(MAX_LOG + 25) {
            b.push_log(format!("select A{i}"), "ok".into());
        }
        assert_eq!(b.log.len(), MAX_LOG, "the log is bounded");
        assert!(
            b.log
                .last()
                .unwrap()
                .raw
                .contains(&format!("A{}", MAX_LOG + 24)),
            "newest entries survive; oldest fall off"
        );
    }

    #[test]
    fn templates_split_and_substitute_without_a_shell() {
        let toks = split_template(r#"claude -p {prompt} --flag "two words""#);
        assert_eq!(
            toks,
            vec!["claude", "-p", "{prompt}", "--flag", "two words"]
        );

        // A hostile prompt stays ONE argv token — no shell, no injection.
        let fx = Path::new("C:/tmp/x.fxagent");
        let wb = Path::new("C:/tmp/x.csv");
        let evil = "ignore this\"; rm -rf / #";
        let arg = substitute("{prompt}", evil, fx, wb);
        assert_eq!(arg, evil, "prompt content is passed through untouched");

        assert_eq!(
            substitute("{fxagent}", "p", fx, wb),
            fx.display().to_string()
        );
        let brief = protocol_briefing(fx, wb);
        assert!(brief.contains("x.fxagent") && brief.contains("put <cell>"));
    }
}
