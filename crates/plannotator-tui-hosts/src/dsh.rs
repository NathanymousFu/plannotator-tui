//! `DeepSeek` Harness (`dsh`) sessions:
//! `$DSH_HOME/sessions/--<encoded cwd>--/<session id>/session.v3.jsonl.zstd`.
//!
//! One directory per session, named by the session's own id — the `session-` prefixed id
//! 0.x used, the bare uuid 0.10 uses — under the same encoded-cwd bucket pi files sessions
//! into. Two things make the file unusual. It is appended under **one zstd frame per
//! flush**, so the reader walks the frame chain instead of stopping at the first frame; and
//! its events are dsh's own (`assistant/message`, `user/message`), not a wire format another
//! agent shares. Verified against dsh 0.10.2 (`@deepseek-ai/dsh`: the session store, the
//! `session.v3.jsonl.zstd` name, and the event set).

use std::cmp::Reverse;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ruzstd::decoding::StreamingDecoder;
use serde_json::Value;

use crate::{HostError, Message, Role};

/// The session file names dsh has used, newest scheme first.
pub const SESSION_FILES: [&str; 2] = ["session.v3.jsonl.zstd", "session.jsonl.zstd"];

/// The newest session for `cwd` that holds a reply. Newest first by the transcript's own
/// mtime, which dsh updates on every append — a session opened later but still empty is
/// skipped, not chosen.
pub fn find_transcript(sessions_dir: &Path, cwd: &Path) -> Option<PathBuf> {
    let mut candidates = session_files(&sessions_dir.join(crate::pi::encoded_dir(cwd)));
    candidates.sort_by_key(|path| Reverse(modified(path)));
    candidates.into_iter().find(|path| holds_a_reply(path))
}

/// Find a session by the id its directory carries, in the cwd's bucket first, then in any
/// bucket: Herdr can hand us an id for a pane whose cwd we no longer know.
pub fn find_transcript_by_id(
    sessions_dir: &Path,
    cwd: &Path,
    session_id: &str,
) -> Result<Option<PathBuf>, HostError> {
    let id = crate::validate_session_id(session_id)?;
    let preferred = sessions_dir.join(crate::pi::encoded_dir(cwd)).join(id);
    if let Some(path) = session_file_in(&preferred) {
        return Ok(Some(path));
    }
    let mut buckets: Vec<PathBuf> = std::fs::read_dir(sessions_dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect())
        .unwrap_or_default();
    buckets.sort();
    Ok(buckets.into_iter().map(|bucket| bucket.join(id)).find_map(|dir| session_file_in(&dir)))
}

/// The session id a transcript's directory carries, verbatim: `359c0903-…` for a 0.10
/// session, `session-359c0903-…` for a 0.x one. The directory is what dsh names a session
/// by, so anything that is not uuid-shaped is not an id.
pub fn session_id(transcript: &Path) -> Option<String> {
    let name = transcript.parent()?.file_name()?.to_str()?;
    let candidate = name.strip_prefix("session-").unwrap_or(name);
    crate::is_uuid(candidate).then(|| name.to_owned())
}

/// The transcript as JSONL text: every zstd frame in the file, in order.
pub fn read_transcript(path: &Path) -> Result<String, HostError> {
    let bytes = std::fs::read(path)?;
    Ok(String::from_utf8_lossy(&decode_frames(path, &bytes)?).into_owned())
}

/// dsh appends a frame per flush; `StreamingDecoder` decodes one frame per instance, so the
/// only correct read is frame by frame until the file is consumed. A frame that consumes no
/// bytes would loop forever, so that is an error rather than a hang.
fn decode_frames(path: &Path, bytes: &[u8]) -> Result<Vec<u8>, HostError> {
    let mut jsonl = Vec::new();
    let mut rest: &[u8] = bytes;
    while !rest.is_empty() {
        let before = rest.len();
        {
            let mut decoder = StreamingDecoder::new(&mut rest).map_err(|err| not_zstd(path, &err))?;
            decoder.read_to_end(&mut jsonl).map_err(|err| not_zstd(path, &err))?;
        }
        if rest.len() == before {
            return Err(not_zstd(path, &format!("a frame read none of its {before} bytes")));
        }
    }
    Ok(jsonl)
}

fn not_zstd(path: &Path, err: &dyn std::fmt::Display) -> HostError {
    use std::io::ErrorKind;
    let message = format!("{} is not a zstd session: {err}", path.display());
    HostError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
}

/// The newest `n` messages, newest first: every text block of an event that carries text,
/// with dsh's reasoning and tool blocks left out. One event is one message, as in pi.
pub fn parse_messages(jsonl: &str, n: usize) -> Vec<Message> {
    jsonl.lines().filter_map(event_message).rev().take(n).collect()
}

fn event_message(line: &str) -> Option<Message> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let (role, blocks) = match value.get("type")?.as_str()? {
        "assistant/message" => (Role::Assistant, value.pointer("/data/message/content")?.as_array()?),
        "user/message" => (Role::Human, value.pointer("/data/content")?.as_array()?),
        _ => return None,
    };
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join("\n");
    if text.trim().is_empty() {
        return None;
    }
    let id = value.pointer("/data/message/id").and_then(Value::as_str).map_or_else(
        || format!("seq-{}", value.get("seq").and_then(Value::as_u64).unwrap_or(0)),
        str::to_owned,
    );
    let at = value.get("time").and_then(Value::as_u64).map(crate::time::iso_from_unix_ms);
    Some(Message { id, role, text, at })
}

/// Whether this file is a whole session with a reply rather than a subagent's or a session
/// that was only opened: dsh files one session per delegated child, and those are not the
/// reply the pane is showing.
fn holds_a_reply(path: &Path) -> bool {
    let Ok(text) = read_transcript(path) else { return false };
    !delegated(&text) && !parse_messages(&text, 1).is_empty()
}

/// A `session` header with a delegation depth above zero belongs to a subagent.
fn delegated(jsonl: &str) -> bool {
    let Some(line) = jsonl.lines().next() else { return false };
    let Ok(header) = serde_json::from_str::<Value>(line.trim()) else { return false };
    header.get("delegationDepth").and_then(Value::as_u64).is_some_and(|depth| depth > 0)
}

/// Every session file in a bucket, in a stable order apart from mtime.
fn session_files(bucket: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(bucket) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> =
        entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
    dirs.sort();
    dirs.into_iter().filter_map(|dir| session_file_in(&dir)).collect()
}

/// The session file inside one session directory, under the first name dsh used for it.
fn session_file_in(dir: &Path) -> Option<PathBuf> {
    SESSION_FILES.into_iter().map(|name| dir.join(name)).find(|path| path.is_file())
}

fn modified(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_nanos() as u64)
}
