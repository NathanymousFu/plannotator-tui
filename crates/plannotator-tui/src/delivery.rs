//! Where rendered feedback goes when the user sends it (decision 11).
//!
//! The app renders feedback with one function regardless of target; only the target
//! varies. Clipboard is the standalone default. Inside Herdr the target is an agent's pane
//! and the transport is `herdr agent prompt`. Nothing else in the app knows which is in use;
//! it only distinguishes the three outcomes below.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Why a send did not land. The app reacts differently to each.
#[derive(Debug)]
pub(crate) enum DeliveryError {
    /// The agent is at a dialog and Herdr refused the prompt; retry later.
    Blocked(String),
    /// No agent to send to: pane gone, agent not detected yet, herdr binary missing.
    Unavailable(String),
    /// Herdr refused before typing anything because it cannot drive this agent: an agent
    /// that reports its own lifecycle (see `report-agent`) has no manifest telling
    /// `agent prompt` how to type into it. The pane itself may still take the text.
    Unpromptable(String),
    /// Anything else, with whatever the transport said.
    Failed(anyhow::Error),
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked(msg) | Self::Unavailable(msg) | Self::Unpromptable(msg) => f.write_str(msg),
            Self::Failed(err) => write!(f, "{err:#}"),
        }
    }
}

impl From<std::io::Error> for DeliveryError {
    fn from(err: std::io::Error) -> Self {
        Self::Failed(err.into())
    }
}

pub(crate) trait Delivery {
    /// Shown in the footer and on the button, e.g. `clipboard` or `claude in w1:p1`.
    fn describe(&self) -> String;
    /// True when the target is an agent that will act on the feedback.
    fn is_agent(&self) -> bool {
        false
    }
    /// The receiving agent's host label (`claude`, `codex`, ...), when the target is one.
    fn agent_host(&self) -> Option<&str> {
        None
    }
    fn deliver(&self, feedback: &str) -> Result<(), DeliveryError>;
}

/// The OSC 52 sequence that hands `text` to the terminal's clipboard.
///
/// The terminal the app draws on is the one the person is sitting at, so a copy lands on their
/// machine even when the app itself runs on a remote server: Herdr 0.9.0 forwards a pane's OSC 52
/// to the viewing client. Terminals commonly refuse a base64 payload over 74994 bytes; the sequence
/// is still emitted whole, because truncating a copy silently is worse than one the terminal drops.
pub(crate) fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", crate::base64::encode(text.as_bytes()))
}

/// OSC 52: hand text to the terminal's clipboard so Cmd-V works outside the app.
#[derive(Debug, Default)]
pub(crate) struct Clipboard;

impl Delivery for Clipboard {
    fn describe(&self) -> String {
        "clipboard".to_owned()
    }

    fn deliver(&self, feedback: &str) -> Result<(), DeliveryError> {
        let mut out = std::io::stdout().lock();
        // Callers write between frames, so the sequence never lands inside one.
        out.write_all(osc52_sequence(feedback).as_bytes())?;
        out.flush()?;
        Ok(())
    }
}

/// Headless runs: nothing leaves the process.
#[derive(Debug, Default)]
pub(crate) struct Discard;

impl Delivery for Discard {
    fn describe(&self) -> String {
        "nowhere (headless)".to_owned()
    }

    fn deliver(&self, _feedback: &str) -> Result<(), DeliveryError> {
        Ok(())
    }
}

/// An agent running in a Herdr pane. `herdr agent prompt` pastes the feedback as the
/// agent's next message and presses Enter.
#[derive(Debug)]
pub(crate) struct HerdrAgent {
    bin: PathBuf,
    pane: String,
    agent: Option<String>,
}

impl HerdrAgent {
    pub(crate) fn new(bin: PathBuf, pane: String, agent: Option<String>) -> Self {
        Self { bin, pane, agent }
    }

    /// `herdr pane run <pane> <text>`: paste the feedback and submit it without asking
    /// Herdr to know the agent. Bracketed paste keeps a multi-line body one paste plus one
    /// Enter. Used only for an agent `agent prompt` refuses to drive.
    fn paste_into_pane(&self, feedback: &str) -> Result<(), DeliveryError> {
        let output = Command::new(&self.bin)
            .args(["pane", "run", &self.pane, feedback])
            .stdin(Stdio::null())
            .output()
            .map_err(|err| DeliveryError::Unavailable(format!("cannot run {}: {err}", self.bin.display())))?;
        if output.status.success() {
            return Ok(());
        }
        let message = error_message(&String::from_utf8_lossy(&output.stderr));
        Err(DeliveryError::Failed(anyhow::anyhow!("herdr pane run failed: {message}")))
    }

    /// Whether the pane still hosts an agent (`herdr agent get`). Pasting into a pane that
    /// does not is the one thing this fallback must never do: a shell would run the
    /// feedback as a command.
    fn pane_hosts_an_agent(&self) -> bool {
        Command::new(&self.bin)
            .args(["agent", "get", &self.pane])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success() && listed_agent(&output.stdout).is_some())
    }
}

/// The `agent` name in an `agent get` reply; a pane without an agent answers with an
/// error envelope instead.
fn listed_agent(stdout: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let agent = json.pointer("/result/agent/agent")?.as_str()?.trim().to_owned();
    (!agent.is_empty()).then_some(agent)
}

impl Delivery for HerdrAgent {
    fn agent_host(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    fn describe(&self) -> String {
        match &self.agent {
            Some(agent) => format!("{agent} in {}", self.pane),
            None => self.pane.clone(),
        }
    }

    fn is_agent(&self) -> bool {
        true
    }

    fn deliver(&self, feedback: &str) -> Result<(), DeliveryError> {
        let output = Command::new(&self.bin)
            .args(["agent", "prompt", &self.pane, feedback])
            .stdin(Stdio::null())
            .output()
            .map_err(|err| DeliveryError::Unavailable(format!("cannot run {}: {err}", self.bin.display())))?;
        let classified = parse_response(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
        match classified {
            // Herdr checked the pane and typed nothing, so the text is still ours to
            // place. A dialog is different: Herdr refused on purpose, and pasting into it
            // would answer the dialog instead of delivering feedback.
            Err(DeliveryError::Unpromptable(_)) if self.pane_hosts_an_agent() => {
                self.paste_into_pane(feedback)
            }
            outcome => outcome,
        }
    }
}

/// Map a `herdr agent prompt` exit into an outcome. Herdr prints a JSON envelope
/// `{"error":{"code","message"}}` on stderr when it refuses.
pub(crate) fn parse_response(success: bool, _stdout: &str, stderr: &str) -> Result<(), DeliveryError> {
    if success {
        return Ok(());
    }
    let code = error_code(stderr);
    let message = error_message(stderr);
    match code.as_deref() {
        Some("agent_blocked") => Err(DeliveryError::Blocked(message)),
        // Herdr found the pane and typed nothing: an agent it cannot drive. Whoever called
        // decides whether the pane itself can take the text instead.
        Some("agent_not_found" | "agent_not_ready") => Err(DeliveryError::Unpromptable(message)),
        Some("pane_not_found" | "empty_agent_prompt") => Err(DeliveryError::Unavailable(message)),
        Some(code) => Err(DeliveryError::Failed(anyhow::anyhow!("{code}: {message}"))),
        None => Err(DeliveryError::Failed(anyhow::anyhow!(
            "herdr agent prompt failed: {}",
            if message.is_empty() { "no output".to_owned() } else { message }
        ))),
    }
}

/// The `code` of Herdr's JSON error envelope, when it sent one.
fn error_code(stderr: &str) -> Option<String> {
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim()).ok()?;
    Some(envelope.pointer("/error/code")?.as_str()?.to_owned())
}

/// The `message` of Herdr's JSON error envelope, else the raw stderr.
fn error_message(stderr: &str) -> String {
    serde_json::from_str::<serde_json::Value>(stderr.trim())
        .ok()
        .and_then(|envelope| envelope.pointer("/error/message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| stderr.trim().to_owned())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "tests assert by panicking"
)]
mod tests {
    use super::*;

    fn envelope(code: &str, message: &str) -> String {
        format!(r#"{{"id":"cli:agent:prompt","error":{{"code":"{code}","message":"{message}"}}}}"#)
    }

    #[test]
    fn the_clipboard_sequence_is_osc52_over_the_raw_utf8_bytes() {
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
        assert_eq!(
            osc52_sequence("hi").as_bytes(),
            &[0x1b, 0x5d, 0x35, 0x32, 0x3b, 0x63, 0x3b, 0x61, 0x47, 0x6b, 0x3d, 0x07]
        );
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
        assert_eq!(osc52_sequence("한글 · é"), "\x1b]52;c;7ZWc6riAIMK3IMOp\x07");
    }

    #[test]
    fn an_oversized_copy_is_emitted_whole_rather_than_truncated() {
        // 74994 base64 bytes is the payload many terminals stop at; we never cut a copy to fit.
        let text = "a".repeat(80_000);
        let sequence = osc52_sequence(&text);
        assert!(sequence.len() > 74_994);
        assert!(sequence.starts_with("\x1b]52;c;") && sequence.ends_with('\x07'));
        assert!(sequence.contains(&crate::base64::encode(text.as_bytes())));
    }

    /// A stand-in for `herdr`: it refuses `agent prompt` the way Herdr refuses an agent it
    /// cannot drive, answers `agent get` from the case, and logs every call. A shell script
    /// is enough for the unix tests and keeps them off any build step.
    #[cfg(unix)]
    fn fake_herdr(tag: &str, prompt_error: &str, agent_get: (&str, i32)) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("plannotator delivery {tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("calls.log");
        let bin = dir.join("herdr");
        std::fs::write(
            &bin,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s\\n' \"$*\" >> '{log}'\n",
                    "case \"$1 $2\" in\n",
                    "  'agent prompt') printf '%s' '{prompt_error}' >&2; exit 1 ;;\n",
                    "  'agent get') printf '%s' '{agent_get}'; exit {agent_get_exit} ;;\n",
                    "  'pane run') exit 0 ;;\n",
                    "esac\n",
                    "exit 1\n"
                ),
                log = log.display(),
                prompt_error = prompt_error,
                agent_get = agent_get.0,
                agent_get_exit = agent_get.1,
            ),
        )
        .expect("fake herdr");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (bin, log)
    }

    /// The calls the fake recorded, then its directory.
    #[cfg(unix)]
    fn calls(log: &std::path::Path) -> String {
        let text = std::fs::read_to_string(log).expect("call log");
        std::fs::remove_dir_all(log.parent().expect("parent")).expect("cleanup");
        text
    }

    #[cfg(unix)]
    #[test]
    fn an_agent_herdr_cannot_prompt_gets_the_feedback_pasted_into_its_pane() {
        // dsh reports its own lifecycle (`herdr pane report-agent`), so Herdr has no
        // manifest to drive it and refuses the prompt; the pane is the only way in.
        let (bin, log) = fake_herdr(
            "custom agent",
            r#"{"error":{"code":"agent_not_ready","message":"agent w1:p1 is not ready for prompts"}}"#,
            (r#"{"result":{"agent":{"agent":"dsh-tui","pane_id":"w1:p1"}}}"#, 0),
        );
        let outcome = HerdrAgent::new(bin, "w1:p1".into(), Some("dsh-tui".into())).deliver("feedback text");

        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            calls(&log),
            "agent prompt w1:p1 feedback text\nagent get w1:p1\npane run w1:p1 feedback text\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_pane_without_an_agent_is_never_pasted_into() {
        // A shell would run the feedback as a command, so the refusal stands.
        let (bin, log) = fake_herdr(
            "no agent",
            r#"{"error":{"code":"agent_not_found","message":"agent target w1:p1 not found"}}"#,
            (r#"{"error":{"code":"agent_not_found","message":"agent target w1:p1 not found"}}"#, 1),
        );
        let err = HerdrAgent::new(bin, "w1:p1".into(), None).deliver("feedback text").expect_err("refused");

        assert!(matches!(err, DeliveryError::Unpromptable(_)), "{err:?}");
        let calls = calls(&log);
        assert!(calls.contains("agent get w1:p1"), "{calls}");
        assert!(!calls.contains("pane run"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn a_dialog_is_not_answered_by_pasting_feedback_into_it() {
        // Herdr refused because the agent is at a dialog: pasting would answer the dialog.
        let (bin, log) = fake_herdr(
            "dialog",
            r#"{"error":{"code":"agent_blocked","message":"agent w1:p1 is blocked"}}"#,
            (r#"{"result":{"agent":{"agent":"dsh-tui","pane_id":"w1:p1"}}}"#, 0),
        );
        let err = HerdrAgent::new(bin, "w1:p1".into(), Some("dsh-tui".into()))
            .deliver("feedback text")
            .expect_err("refused");

        assert!(matches!(&err, DeliveryError::Blocked(m) if m == "agent w1:p1 is blocked"), "{err:?}");
        assert_eq!(calls(&log), "agent prompt w1:p1 feedback text\n");
    }

    #[test]
    fn headless_runs_never_reach_the_terminal_clipboard() {
        // `--print`, `--export` and `--snapshot` open the app non-interactively, and a freshly
        // opened app has not enabled clipboard copies; only the interactive event loop does.
        assert_eq!(crate::cli::delivery(false).describe(), Discard.describe());
        let source =
            plannotator_tui_schema::DocumentSource::file(PathBuf::from("doc.md"), "# hi\n".to_owned());
        let app = crate::app::App::open(source, 80, crate::cli::delivery(false)).expect("open");
        assert!(!app.clipboard);
    }

    #[test]
    fn success_is_ok_regardless_of_output() {
        assert!(parse_response(true, r#"{"id":"x","result":{}}"#, "").is_ok());
    }

    #[test]
    fn blocked_agent_is_blocked_with_herdrs_message() {
        let err =
            parse_response(false, "", &envelope("agent_blocked", "agent w1:p1 is blocked")).unwrap_err();
        assert!(matches!(&err, DeliveryError::Blocked(m) if m == "agent w1:p1 is blocked"));
    }

    #[test]
    fn an_agent_herdr_cannot_drive_is_a_refusal_before_typing() {
        // Herdr found the pane and typed nothing, so the caller may paste into the pane.
        for code in ["agent_not_found", "agent_not_ready"] {
            let err = parse_response(false, "", &envelope(code, "not ready for prompts")).unwrap_err();
            assert!(matches!(&err, DeliveryError::Unpromptable(m) if m == "not ready for prompts"), "{code}");
        }
    }

    #[test]
    fn a_gone_pane_or_an_empty_prompt_is_unavailable() {
        for code in ["pane_not_found", "empty_agent_prompt"] {
            let err = parse_response(false, "", &envelope(code, "gone")).unwrap_err();
            assert!(matches!(err, DeliveryError::Unavailable(_)), "{code}");
        }
    }

    #[test]
    fn an_agent_get_reply_names_the_agent_only_when_one_is_there() {
        let listed = br#"{"result":{"agent":{"agent":"dsh-tui","pane_id":"w1:p1"}}}"#;
        assert_eq!(listed_agent(listed).as_deref(), Some("dsh-tui"));
        let refused = br#"{"error":{"code":"agent_not_found","message":"none"}}"#;
        assert_eq!(listed_agent(refused), None);
        assert_eq!(listed_agent(br#"{"result":{"agent":{"agent":"  "}}}"#), None);
        assert_eq!(listed_agent(b"not json"), None);
    }

    #[test]
    fn unknown_code_and_garbage_are_failures_that_keep_the_text() {
        let err = parse_response(false, "", &envelope("socket_error", "boom")).unwrap_err();
        assert!(matches!(&err, DeliveryError::Failed(e) if e.to_string() == "socket_error: boom"));
        let err = parse_response(false, "", "connection refused\n").unwrap_err();
        assert!(matches!(&err, DeliveryError::Failed(e) if e.to_string().contains("connection refused")));
    }

    #[test]
    fn describe_names_the_agent_when_known() {
        let bin = PathBuf::from("herdr");
        assert_eq!(
            HerdrAgent::new(bin.clone(), "w1:p1".into(), Some("claude".into())).describe(),
            "claude in w1:p1"
        );
        assert_eq!(HerdrAgent::new(bin, "w1:p1".into(), None).describe(), "w1:p1");
    }

    #[cfg(windows)]
    #[test]
    fn windows_create_process_keeps_feedback_in_one_argument() {
        let root = std::env::var_os("RUNNER_TEMP")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .join(format!("plannotator delivery proof ü-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        let fake = root.join("fake herdr.exe");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-herdr.rs");
        let output = Command::new("rustc")
            .arg("--edition=2024")
            .arg(source)
            .arg("-o")
            .arg(&fake)
            .output()
            .expect("run rustc");
        assert!(output.status.success(), "rustc failed: {}", String::from_utf8_lossy(&output.stderr));
        let feedback = "line one\n\"quoted\" 100% & ready | café 中文";
        HerdrAgent::new(fake, "w1:p1".into(), Some("codex".into())).deliver(feedback).expect("delivered");
        let log = std::fs::read_to_string(root.join("calls.jsonl")).expect("call log");
        let call: serde_json::Value = serde_json::from_str(log.trim()).expect("JSON call");
        assert_eq!(call["argv"], serde_json::json!(["agent", "prompt", "w1:p1", feedback]));
        assert_eq!(call["argv"].as_array().expect("argv").len(), 4);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
