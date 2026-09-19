//! dsh sessions: what counts as a message, the zstd frame chain, and finding a session.
//! The fixture store is dsh's own layout, verified against dsh 0.10.2 on this machine
//! (`$DSH_HOME/sessions/--<encoded cwd>--/<session id>/session.v3.jsonl.zstd`).

#![allow(clippy::expect_used, clippy::indexing_slicing, reason = "tests assert by panicking")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use plannotator_tui_hosts::dsh::{
    find_transcript, find_transcript_by_id, parse_messages, read_transcript, session_id,
};
use plannotator_tui_hosts::pi::encoded_dir;
use plannotator_tui_hosts::{Host, Role, session_id_of};

const PROJECT: &str = "11111111-1111-4111-8111-111111111111";
const EMPTY: &str = "22222222-2222-4222-8222-222222222222";
const SUBAGENT: &str = "33333333-3333-4333-8333-333333333333";
const LEGACY: &str = "session-44444444-4444-4444-8444-444444444444";

struct Store {
    root: PathBuf,
}

impl Store {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("plannotator-tui-hosts-dsh-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        copy_dir(&fixtures(), &root);
        Self { root }
    }

    /// The session file for a bucket and id, aged `age_secs` seconds.
    fn session(&self, bucket: &str, id: &str, name: &str, age_secs: u64) -> PathBuf {
        let path = self.root.join(bucket).join(id).join(name);
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        // Windows needs write access to change a timestamp.
        fs::OpenOptions::new().write(true).open(&path).expect("open").set_modified(when).expect("mtime");
        path
    }
}

fn fixtures() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dsh/sessions")).to_path_buf()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("dir");
    for entry in fs::read_dir(from).expect("read").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn texts(role: Role, messages: &[plannotator_tui_hosts::Message]) -> Vec<String> {
    messages.iter().filter(|m| m.role == role).map(|m| m.text.clone()).collect()
}

fn fixture_messages() -> Vec<plannotator_tui_hosts::Message> {
    let path = fixtures().join("--work-project--").join(PROJECT).join("session.v3.jsonl.zstd");
    parse_messages(&read_transcript(&path).expect("decodes"), 25)
}

#[test]
fn every_text_block_of_an_event_is_one_message_newest_first() {
    let messages = fixture_messages();
    assert_eq!(texts(Role::Assistant, &messages), vec!["second reply", "first reply"]);
    assert_eq!(texts(Role::Human, &messages), vec!["user prompt one"]);
    assert_eq!(messages[0].id, "a2", "the event's own message id names the message");
    assert_eq!(messages[0].at.as_deref(), Some("2026-08-28T17:27:40.000Z"));
    assert_eq!(messages[2].id, "seq-8", "an event without an id falls back to its sequence");
}

#[test]
fn reasoning_and_tool_blocks_are_not_messages() {
    let messages = fixture_messages();
    assert!(
        messages.iter().all(|m| !m.text.contains("thinking") && !m.text.contains("file_path")),
        "a step whose content is only reasoning is not a message either"
    );
}

#[test]
fn n_caps_the_total() {
    assert_eq!(fixture_messages().len(), 3);
    let path = fixtures().join("--work-project--").join(PROJECT).join("session.v3.jsonl.zstd");
    assert_eq!(parse_messages(&read_transcript(&path).expect("decodes"), 1).len(), 1);
}

#[test]
fn the_frame_chain_decodes_every_frame_not_just_the_first() {
    // dsh appends one frame per flush, so the last messages live in later frames.
    let path = fixtures().join("--work-project--").join(PROJECT).join("session.v3.jsonl.zstd");
    let jsonl = read_transcript(&path).expect("decodes");
    assert_eq!(jsonl.lines().count(), 7, "both frames, no frame repeated");
    assert!(jsonl.contains("user prompt one"), "the first frame");
    assert!(jsonl.contains("second reply"), "the appended frame");
}

#[test]
fn a_file_that_is_not_a_zstd_session_is_an_error_not_a_panic() {
    let dir = std::env::temp_dir().join(format!("plannotator-tui-hosts-dsh-garbage-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("session.v3.jsonl.zstd");
    fs::write(&path, b"not zstd at all").expect("write");
    let err = read_transcript(&path).expect_err("refuses");
    assert!(err.to_string().contains("not a zstd session"), "{err}");
    fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn the_newest_session_holding_a_reply_wins_over_later_empty_and_subagent_sessions() {
    let store = Store::new("newest");
    // The one opened last is empty, the subagent's is newest of all: neither is the reply.
    store.session("--work-project--", EMPTY, "session.v3.jsonl.zstd", 20);
    store.session("--work-project--", SUBAGENT, "session.v3.jsonl.zstd", 10);
    let expected = store.session("--work-project--", PROJECT, "session.v3.jsonl.zstd", 30);

    let found = find_transcript(&store.root, Path::new("/work/project")).expect("a session");
    assert_eq!(found, expected);
    assert_eq!(encoded_dir(Path::new("/work/project")), "--work-project--", "dsh's own bucket");
    fs::remove_dir_all(&store.root).expect("cleanup");
}

#[test]
fn a_bucket_without_a_session_yields_nothing() {
    let store = Store::new("missing");
    assert_eq!(find_transcript(&store.root, Path::new("/work/elsewhere")), None);
    fs::remove_dir_all(&store.root).expect("cleanup");
}

#[test]
fn a_session_is_found_by_the_id_its_directory_carries() {
    let store = Store::new("by id");
    let found = find_transcript_by_id(&store.root, Path::new("/work/project"), PROJECT).expect("looks");
    assert!(found.is_some_and(|path| path.ends_with("session.v3.jsonl.zstd")));
    // A 0.x session keeps the `session-` prefix in its id and its file name.
    assert!(
        find_transcript_by_id(&store.root, Path::new("/work/legacy"), LEGACY)
            .expect("looks")
            .is_some_and(|path| path.ends_with("session.jsonl.zstd"))
    );
    assert!(
        find_transcript_by_id(
            &store.root,
            Path::new("/work/project"),
            "99999999-9999-4999-8999-999999999999"
        )
        .expect("looks")
        .is_none()
    );
    assert!(find_transcript_by_id(&store.root, Path::new("/work"), "../etc").is_err(), "not an id");
    fs::remove_dir_all(&store.root).expect("cleanup");
}

#[test]
fn a_transcript_names_its_session_by_its_directory() {
    let project = fixtures().join("--work-project--").join(PROJECT).join("session.v3.jsonl.zstd");
    assert_eq!(session_id(&project).as_deref(), Some(PROJECT));
    assert_eq!(session_id_of(Host::Dsh, &project).as_deref(), Some(PROJECT));
    let legacy = fixtures().join("--work-legacy--").join(LEGACY).join("session.jsonl.zstd");
    assert_eq!(session_id_of(Host::Dsh, &legacy).as_deref(), Some(LEGACY));
    let unnamed = fixtures().join("--work-project--").join(EMPTY).join("session.v3.jsonl.zstd");
    assert_eq!(session_id_of(Host::Dsh, &unnamed.join("..")), None, "a name that is not an id");
}
