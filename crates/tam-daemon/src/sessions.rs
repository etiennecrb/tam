//! Claude's session files, read for chats: a session's title and last
//! prompt, and the sessions of a repository's main checkout (its history,
//! what a chat can be made from).
//!
//! Claude writes a session to `<claude dir>/projects/<project>/<id>.jsonl`,
//! `<project>` being the directory it runs in, every character but ASCII
//! letters and digits made a `-` (cut at 200, plus a hash, when longer:
//! [`project_name`]). The format is Claude's own and undocumented; this
//! module is the only one that reads it, and a file it cannot read gives
//! nothing, never an error. A file is one JSON object per line; the lines
//! read here (as Claude 2.1 writes them):
//!
//! - `{"type":"ai-title","aiTitle":…,"sessionId":…}`: the title Claude
//!   generates, written again as the conversation moves;
//! - `{"type":"custom-title","customTitle":…,"sessionId":…}`: the name the
//!   user gave the session in Claude (`/rename`, or the desktop app's
//!   rename); an empty one takes it back;
//! - `{"type":"last-prompt","lastPrompt":…,"leafUuid":…,"sessionId":…}`:
//!   the latest prompt typed, on one line, cut at 200 characters.
//!
//! The latest line of each kind wins, and Claude shows the user's name
//! before its own title. A file can be tens of megabytes: Claude writes
//! these lines again at its end every 32 KiB it appends, and reads them
//! from the last 64 KiB, then the first 64 KiB. So does this module
//! ([`WINDOW`]).
//!
//! pi's are read too, for its chats: `<pi dir>/sessions/<project>/<timestamp>_<id>.jsonl`,
//! `<project>` the directory it ran in with `/`, `\` and `:` made `-` between
//! `--` ([`pi_project_name`]), or one directory for every project when
//! `$PI_CODING_AGENT_SESSION_DIR` is set. Its first line is the header (`id`,
//! `cwd`); the lines read here are `{"type":"session_info","name":…}` (the
//! name the user gave the session; pi generates no title) and its messages,
//! the last `{"type":"message","message":{"role":"user",…}}` being the latest
//! prompt. pi appends like Claude, so the same window is read.
//!
//! Known limit, as for transcripts elsewhere in the daemon: the files are
//! looked for under the daemon's own `HOME`/`CLAUDE_CONFIG_DIR`/`PI_CODING_AGENT_DIR`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use tam_proto::SessionInfo;

/// How much of a file's end (and, for what is not there, of its start) is
/// read: what Claude reads for its own session list.
pub const WINDOW: u64 = 64 * 1024;

/// Longest title or prompt kept, in characters (Claude's own cut for a
/// prompt).
const MAX_TEXT: usize = 200;

/// Longest project name before Claude cuts it and adds a hash.
const MAX_PROJECT_NAME: usize = 200;

/// Claude's data directory: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")),
    }
}

/// The directory of Claude's sessions run in `dir`.
pub fn project_dir(claude_dir: &Path, dir: &Path) -> PathBuf {
    claude_dir.join("projects").join(project_name(dir))
}

/// How Claude names the project of `dir`: every UTF-16 unit but ASCII
/// letters and digits made a `-`; past 200, cut there and followed by `-`
/// and a hash of the path (base 36).
pub fn project_name(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    let mut name = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else {
            name.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    if name.len() <= MAX_PROJECT_NAME {
        return name;
    }
    // ASCII only by now: bytes are characters.
    name.truncate(MAX_PROJECT_NAME);
    format!("{name}-{}", base36(path_hash(&path).unsigned_abs().into()))
}

/// The 32-bit string hash Claude takes of a long project path
/// (`h = h * 31 + unit`, over UTF-16 units).
fn path_hash(path: &str) -> i32 {
    path.encode_utf16()
        .fold(0i32, |h, unit| h.wrapping_mul(31).wrapping_add(unit.into()))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII digits")
}

/// The file of session `session` run in `dir`, whether it exists or not.
/// `None` for what cannot be a session id (empty, a path).
pub fn claude_session_file(claude_dir: &Path, dir: &Path, session: &str) -> Option<PathBuf> {
    if !valid_session(session) {
        return None;
    }
    Some(project_dir(claude_dir, dir).join(format!("{session}.jsonl")))
}

/// What Claude's file of a session says of it: its title (the user's name
/// for it, else Claude's), its latest prompt, and when it was last written.
/// `None` when it cannot be read.
pub fn claude_session(file: &Path) -> Option<SessionInfo> {
    let id = file
        .file_name()?
        .to_str()?
        .strip_suffix(".jsonl")
        .filter(|id| !id.is_empty() && !id.starts_with('.'))?;
    let (mut f, len, modified_at) = open_session(file)?;
    let start = len.saturating_sub(WINDOW);
    let mut found = Found::default();
    found.scan(&read_at(&mut f, start, len - start)?, start > 0, id);
    // A file the end did not hold all of: what is missing may be at its
    // start, as Claude looks for it.
    if start > 0 && !found.complete() {
        let mut head = Found::default();
        head.scan(&read_at(&mut f, 0, WINDOW.min(start))?, false, id);
        found.fill_from(head);
    }
    Some(SessionInfo {
        id: id.to_string(),
        title: found.title(),
        last_prompt: found.last_prompt.as_deref().and_then(clean),
        modified_at,
    })
}

/// Claude's sessions of `dir` (a repository's main checkout), newest first.
pub fn claude_history(claude_dir: &Path, dir: &Path) -> Vec<SessionInfo> {
    newest_first(
        jsonl_files(&project_dir(claude_dir, dir))
            .iter()
            .filter_map(|file| claude_session(file))
            .collect(),
    )
}

/// The session files under `dir`, in no order.
fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect()
}

/// The sessions to list, newest first. A session nobody typed in says
/// nothing worth listing (a new chat's, before its first turn).
fn newest_first(mut sessions: Vec<SessionInfo>) -> Vec<SessionInfo> {
    sessions.retain(|session| session.title.is_some() || session.last_prompt.is_some());
    sessions.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    sessions
}

/// A session's file, open, with its length and when it was last written
/// (Unix seconds). `None` when it cannot be read.
fn open_session(file: &Path) -> Option<(File, u64, u64)> {
    let f = File::open(file).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some((f, meta.len(), modified_at))
}

// --- pi ---

/// pi's data directory: `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn pi_dir() -> Option<PathBuf> {
    match std::env::var_os("PI_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(
            PathBuf::from(std::env::var_os("HOME")?)
                .join(".pi")
                .join("agent"),
        ),
    }
}

/// `$PI_CODING_AGENT_SESSION_DIR`: one directory for every project, when set.
fn pi_session_dir() -> Option<PathBuf> {
    std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// How pi names the directory of the sessions run in `dir`: one leading
/// separator dropped, `/`, `\` and `:` made `-`, between `--`
/// (`--Users-me-app--`).
pub fn pi_project_name(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    let stripped = path.strip_prefix(['/', '\\']).unwrap_or(path.as_ref());
    let name: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c => c,
        })
        .collect();
    format!("--{name}--")
}

/// The directory holding pi's sessions of `dir`: its project's, or the one
/// `$PI_CODING_AGENT_SESSION_DIR` names (every project in it, a file's
/// header saying where it ran).
fn pi_project_dir(dir: &Path) -> Option<PathBuf> {
    match pi_session_dir() {
        Some(flat) => Some(flat),
        None => Some(pi_dir()?.join("sessions").join(pi_project_name(dir))),
    }
}

/// The header of pi's session file (its first line): the session's id and
/// the directory it runs in.
fn pi_header(bytes: &[u8]) -> Option<(String, Option<PathBuf>)> {
    #[derive(Deserialize)]
    struct Header {
        #[serde(rename = "type")]
        kind: String,
        id: Option<String>,
        cwd: Option<String>,
    }
    let line = bytes.split(|&b| b == b'\n').next()?;
    let header: Header = serde_json::from_slice(line).ok()?;
    if header.kind != "session" {
        return None;
    }
    Some((header.id?, header.cwd.map(PathBuf::from)))
}

/// How much of pi's file its header (one line) is read from.
const HEAD: u64 = 4 * 1024;

/// The header of the session in `file`.
fn pi_header_of(file: &Path) -> Option<(String, Option<PathBuf>)> {
    let mut f = File::open(file).ok()?;
    pi_header(&read_at(&mut f, 0, HEAD)?)
}

/// Whether pi ran the session in `file` in `dir`. `own`: the file is in
/// `dir`'s own project directory (not the one every project is in), where
/// every session ran in it.
fn pi_ran_in(file: &Path, dir: &Path, own: bool) -> bool {
    own || pi_header_of(file)
        .and_then(|(_, cwd)| cwd)
        .is_some_and(|cwd| tam_worktree::worktree::same_path(&cwd, dir))
}

/// Whether `project` is the directory pi keeps the sessions run in `dir` in.
fn pi_owns(project: &Path, dir: &Path) -> bool {
    project
        .file_name()
        .is_some_and(|name| name == pi_project_name(dir).as_str())
}

/// What pi's file of a session says of it: the name the user gave it
/// (`/name`; pi generates no title), the latest prompt typed, and when it
/// was last written. `None` when it cannot be read.
pub fn pi_session(file: &Path) -> Option<SessionInfo> {
    let (mut f, len, modified_at) = open_session(file)?;
    let (id, _cwd) = pi_header(&read_at(&mut f, 0, HEAD.min(len))?)?;
    // pi appends, so the end holds the latest name and prompt; a name set
    // long ago (or a turn that wrote more than a window) may be only at the
    // start.
    let start = len.saturating_sub(WINDOW);
    let mut found = PiFound::default();
    found.scan(&read_at(&mut f, start, len - start)?, start > 0);
    if start > 0 && !found.complete() {
        let mut earlier = PiFound::default();
        earlier.scan(&read_at(&mut f, 0, WINDOW)?, false);
        found.fill_from(earlier);
    }
    Some(SessionInfo {
        id,
        title: found.title.as_deref().and_then(clean),
        last_prompt: found.prompt.as_deref().and_then(clean),
        modified_at,
    })
}

/// pi's sessions of `dir` that have a name or a prompt, newest first: what
/// a chat can be made from.
pub fn pi_history(dir: &Path) -> Vec<SessionInfo> {
    pi_project_dir(dir)
        .map(|project| pi_history_of(&project, dir))
        .unwrap_or_default()
}

/// [`pi_history`] of `project` (pi's directory for `dir`, or the one
/// holding every project: a file's header then says where it ran).
fn pi_history_of(project: &Path, dir: &Path) -> Vec<SessionInfo> {
    let own = pi_owns(project, dir);
    newest_first(
        jsonl_files(project)
            .iter()
            .filter(|file| pi_ran_in(file, dir, own))
            .filter_map(|file| pi_session(file))
            .collect(),
    )
}

/// The file of pi's session `session` run in `dir`, when it is there: pi
/// names its files `<timestamp>_<session id>.jsonl`.
pub fn pi_session_file(dir: &Path, session: &str) -> Option<PathBuf> {
    pi_session_file_of(&pi_project_dir(dir)?, dir, session)
}

/// [`pi_session_file`] in `project`.
fn pi_session_file_of(project: &Path, dir: &Path, session: &str) -> Option<PathBuf> {
    if !valid_session(session) {
        return None;
    }
    let suffix = format!("_{session}.jsonl");
    let own = pi_owns(project, dir);
    jsonl_files(project)
        .into_iter()
        .find(|file| names_session(file, &suffix) && pi_ran_in(file, dir, own))
}

/// Whether `file` is the file of the session whose file name ends `suffix`
/// (`_<id>.jsonl`).
fn names_session(file: &Path, suffix: &str) -> bool {
    file.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
}

/// Whether `dir` (a project's, or the one every project is in) holds pi's
/// session `session`.
fn dir_holds_session(dir: &Path, session: &str) -> bool {
    if !valid_session(session) {
        return false;
    }
    let suffix = format!("_{session}.jsonl");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .any(|file| names_session(&file.path(), &suffix))
}

/// Whether `claude` holds a transcript of session `session`, in any of its
/// project directories (the encoding of a directory is Claude's).
fn claude_has_session(claude: &Path, session: &str) -> bool {
    if !valid_session(session) {
        return false;
    }
    let file = format!("{session}.jsonl");
    let Ok(projects) = std::fs::read_dir(claude.join("projects")) else {
        return false;
    };
    projects
        .filter_map(|e| e.ok())
        .any(|project| project.path().join(&file).is_file())
}

/// Whether Claude has a transcript of session `session` anywhere it keeps
/// them. `true` when Claude's directory is unknown (nothing to look in).
pub fn claude_session_anywhere(session: &str) -> bool {
    match claude_dir() {
        Some(claude) => claude_has_session(&claude, session),
        None => true,
    }
}

/// Whether pi has session `session` anywhere tam looks: in its project's
/// directory, or in the one every project is in. `true` when pi's
/// directory is unknown (nothing to look in).
pub fn pi_session_anywhere(session: &str) -> bool {
    match pi_session_dir() {
        Some(flat) => dir_holds_session(&flat, session),
        None => match pi_dir() {
            Some(pi) => {
                let Ok(projects) = std::fs::read_dir(pi.join("sessions")) else {
                    return false;
                };
                projects.filter_map(|e| e.ok()).any(|project| {
                    project.path().is_dir() && dir_holds_session(&project.path(), session)
                })
            }
            None => true,
        },
    }
}

/// What pi's lines say of a session (see [`pi_session`]): the latest
/// `session_info` and the last user message.
#[derive(Default)]
struct PiFound {
    title: Option<String>,
    /// Whether a `session_info` was seen: a name taken back is not one an
    /// earlier line gives again.
    named: bool,
    prompt: Option<String>,
}

impl PiFound {
    /// Read `bytes`, a part of the file. `mid_line`: it starts inside a
    /// line, which is skipped. A line cut at the end does not parse, and is
    /// skipped too.
    fn scan(&mut self, bytes: &[u8], mid_line: bool) {
        #[derive(Deserialize)]
        struct Line {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            message: Option<Message>,
        }
        #[derive(Deserialize)]
        struct Message {
            #[serde(default)]
            role: Option<String>,
            #[serde(default)]
            content: Option<serde_json::Value>,
        }

        let mut lines = bytes.split(|&b| b == b'\n');
        if mid_line {
            lines.next();
        }
        for line in lines {
            // Most lines are neither: no JSON parsed for them.
            if !contains(line, b"session_info") && !contains(line, b"\"user\"") {
                continue;
            }
            let Ok(line) = serde_json::from_slice::<Line>(line) else {
                continue;
            };
            match line.kind.as_str() {
                "session_info" => {
                    self.named = true;
                    self.title = line.name;
                }
                "message" => {
                    let Some(message) = line.message else {
                        continue;
                    };
                    if message.role.as_deref() != Some("user") {
                        continue;
                    }
                    if let Some(text) = message.content.as_ref().and_then(text_of) {
                        if !text.trim().is_empty() {
                            self.prompt = Some(text);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn complete(&self) -> bool {
        self.named && self.prompt.is_some()
    }

    /// Take from `earlier` (the start of the file) what this has not.
    fn fill_from(&mut self, earlier: PiFound) {
        if !self.named {
            self.title = earlier.title;
            self.named = earlier.named;
        }
        self.prompt = self.prompt.take().or(earlier.prompt);
    }
}

/// The text of a pi message's content: a string, or its text blocks.
fn text_of(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

// --- the agents' files, as chats read them ---

/// What a session's file says of it, in the files of `agent` (`claude` or
/// `pi`): its title, its latest prompt, and when it was last written.
/// `None` for an agent whose files tam does not read.
pub fn read(agent: &str, file: &Path) -> Option<SessionInfo> {
    match agent {
        "claude" => claude_session(file),
        "pi" => pi_session(file),
        _ => None,
    }
}

/// The sessions of `agent` run in `dir` (a repository's main checkout) that
/// have a title or a prompt, newest first: what a chat can be made from.
/// None for an agent whose files tam does not read.
pub fn history(agent: &str, dir: &Path) -> Vec<SessionInfo> {
    match agent {
        "claude" => claude_dir()
            .map(|claude| claude_history(&claude, dir))
            .unwrap_or_default(),
        "pi" => pi_history(dir),
        _ => Vec::new(),
    }
}

/// The file of session `session` run in `dir` by `agent`, when it is there,
/// for an agent whose files tam reads.
pub fn session_file(agent: &str, dir: &Path, session: &str) -> Option<PathBuf> {
    match agent {
        "claude" => {
            let claude = claude_dir()?;
            claude_session_file(&claude, dir, session).filter(|file| file.is_file())
        }
        "pi" => pi_session_file(dir, session),
        _ => None,
    }
}

/// Whether `session` can name a session: not empty, not a path.
fn valid_session(session: &str) -> bool {
    !session.is_empty() && !session.contains(['/', '\\']) && !session.starts_with('.')
}

/// `len` bytes of `f` from `start` (fewer if it shrank meanwhile).
fn read_at(f: &mut File, start: u64, len: u64) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The lines this module reads; any other field is left alone.
#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "lastPrompt")]
    last_prompt: Option<String>,
}

/// The latest of each kind of line in a part of a file, as written (an
/// empty custom title is found: it takes the name back).
#[derive(Default)]
struct Found {
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
}

impl Found {
    /// Read `bytes`, a part of the file of session `id`. `mid_line`: it
    /// starts inside a line, which is skipped. A line cut at the end does
    /// not parse, and is skipped too.
    fn scan(&mut self, bytes: &[u8], mid_line: bool, id: &str) {
        let mut lines = bytes.split(|&b| b == b'\n');
        if mid_line {
            lines.next();
        }
        for line in lines {
            // Most lines are not these: no JSON parsed for them.
            if !contains(line, b"-title") && !contains(line, b"last-prompt") {
                continue;
            }
            let Ok(line) = serde_json::from_slice::<Line>(line) else {
                continue;
            };
            if line.session_id.as_deref().is_some_and(|s| s != id) {
                continue;
            }
            match line.kind.as_str() {
                "custom-title" if line.custom_title.is_some() => {
                    self.custom_title = line.custom_title
                }
                "ai-title" if line.ai_title.is_some() => self.ai_title = line.ai_title,
                "last-prompt" if line.last_prompt.is_some() => self.last_prompt = line.last_prompt,
                _ => {}
            }
        }
    }

    fn complete(&self) -> bool {
        self.custom_title.is_some() && self.ai_title.is_some() && self.last_prompt.is_some()
    }

    /// Take from `earlier` (the start of the file) what this has not.
    fn fill_from(&mut self, earlier: Found) {
        self.custom_title = self.custom_title.take().or(earlier.custom_title);
        self.ai_title = self.ai_title.take().or(earlier.ai_title);
        self.last_prompt = self.last_prompt.take().or(earlier.last_prompt);
    }

    /// The user's name for the session, else Claude's title.
    fn title(&self) -> Option<String> {
        self.custom_title
            .as_deref()
            .and_then(clean)
            .or_else(|| self.ai_title.as_deref().and_then(clean))
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `text` fit to show on one line: control characters made spaces, runs
/// of whitespace one space, trimmed, cut at [`MAX_TEXT`] characters (with
/// `…`). `None` when nothing is left.
fn clean(text: &str) -> Option<String> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let line = words.join(" ");
    if line.is_empty() {
        return None;
    }
    match line.char_indices().nth(MAX_TEXT) {
        Some((cut, _)) => Some(format!("{}…", line[..cut].trim_end())),
        None => Some(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    const S1: &str = "0b7a8f9e-0000-4000-8000-000000000001";

    fn ai(session: &str, title: &str) -> String {
        serde_json::json!({"type": "ai-title", "aiTitle": title, "sessionId": session}).to_string()
    }

    fn custom(session: &str, title: &str) -> String {
        serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": session})
            .to_string()
    }

    fn prompt(session: &str, text: &str) -> String {
        serde_json::json!({
            "type": "last-prompt",
            "lastPrompt": text,
            "leafUuid": "00000000-0000-4000-8000-0000000000aa",
            "sessionId": session,
        })
        .to_string()
    }

    /// A conversation line, as large as wanted (a tool's output).
    fn filler(session: &str, bytes: usize) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": session,
            "message": {"role": "assistant", "content": "x".repeat(bytes)},
        })
        .to_string()
    }

    fn write(file: &Path, lines: &[String]) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut f = File::create(file).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn set_mtime(file: &Path, secs: u64) {
        File::options()
            .append(true)
            .open(file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn session(dir: &Path, id: &str, lines: &[String]) -> SessionInfo {
        let file = dir.join(format!("{id}.jsonl"));
        write(&file, lines);
        claude_session(&file).unwrap()
    }

    #[test]
    fn project_names_are_claudes() {
        assert_eq!(
            project_name(Path::new("/home/user/Workspace/tam")),
            "-home-user-Workspace-tam"
        );
        assert_eq!(project_name(Path::new("/")), "-");
        // Dots, underscores, spaces: all `-` (a hidden directory doubles it).
        assert_eq!(
            project_name(Path::new("/home/u/.local/a_b c/tam")),
            "-home-u--local-a-b-c-tam"
        );
        // One `-` per UTF-16 unit: two for a character outside the BMP.
        assert_eq!(project_name(Path::new("/w/é/🦀/x")), "-w------x");
        // Longer than 200: cut, then a hash of the path. The expected
        // values are computed as Claude's JavaScript computes them.
        let long = format!("/home/user/{}repo", "very-long-directory-name/".repeat(9));
        let name = project_name(Path::new(&long));
        assert_eq!(name.len(), 207);
        assert!(name.ends_with("-very-long-dire-lwzqcn"), "{name}");
        assert_eq!(path_hash(&long), -1325204519);
        let accented = format!("/srv/é/{}", "x".repeat(210));
        assert_eq!(path_hash(&accented), -1887427027);
        assert!(project_name(Path::new(&accented)).ends_with("xxxxx-v7q40j"));
        assert_eq!(base36(0), "0");
        assert_eq!(base36(i32::MIN.unsigned_abs().into()), "zik0zk");
    }

    #[test]
    fn session_files_are_in_the_project_of_their_directory() {
        let claude = Path::new("/c");
        assert_eq!(
            claude_session_file(claude, Path::new("/src/api"), S1),
            Some(PathBuf::from(format!("/c/projects/-src-api/{S1}.jsonl")))
        );
        for bad in ["", "../x", "a/b", ".hidden", "a\\b"] {
            assert_eq!(
                claude_session_file(claude, Path::new("/src/api"), bad),
                None
            );
        }
    }

    #[test]
    fn the_latest_title_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session(
            tmp.path(),
            S1,
            &[
                prompt(S1, "find the leak"),
                ai(S1, "Investigate a leak"),
                filler(S1, 100),
                prompt(S1, "and fix it"),
                ai(S1, "Fix the connection pool leak"),
            ],
        );
        assert_eq!(s.id, S1);
        assert_eq!(s.title.as_deref(), Some("Fix the connection pool leak"));
        assert_eq!(s.last_prompt.as_deref(), Some("and fix it"));
    }

    #[test]
    fn a_file_without_a_title_has_its_prompt_only() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session(tmp.path(), S1, &[prompt(S1, "hello"), filler(S1, 10)]);
        assert_eq!(s.title, None);
        assert_eq!(s.last_prompt.as_deref(), Some("hello"));
        // Nothing at all: nothing, not an error.
        let s = session(tmp.path(), "empty", &[filler("empty", 10)]);
        assert_eq!((s.title, s.last_prompt), (None, None));
        // A last-prompt line without its prompt (Claude writes some) keeps
        // the earlier one.
        let bare = serde_json::json!({"type": "last-prompt", "leafUuid": "x", "sessionId": S1});
        let s = session(tmp.path(), S1, &[prompt(S1, "first"), bare.to_string()]);
        assert_eq!(s.last_prompt.as_deref(), Some("first"));
    }

    #[test]
    fn the_users_name_wins_over_claudes_title_until_taken_back() {
        let tmp = tempfile::tempdir().unwrap();
        // Even an older name wins over a newer generated title.
        let named = [
            ai(S1, "Generated"),
            custom(S1, "deploy friday"),
            ai(S1, "Generated again"),
        ];
        let s = session(tmp.path(), S1, &named);
        assert_eq!(s.title.as_deref(), Some("deploy friday"));
        // An empty name takes it back: Claude's title shows again.
        let mut cleared = named.to_vec();
        cleared.push(custom(S1, ""));
        let s = session(tmp.path(), S1, &cleared);
        assert_eq!(s.title.as_deref(), Some("Generated again"));
    }

    #[test]
    fn a_large_file_is_read_from_its_end_then_its_start() {
        let tmp = tempfile::tempdir().unwrap();
        let big = (2 * WINDOW) as usize;
        // Claude wrote its lines again near the end: the end's win.
        let s = session(
            tmp.path(),
            S1,
            &[
                custom(S1, "old name"),
                ai(S1, "Old title"),
                prompt(S1, "old prompt"),
                filler(S1, big),
                filler(S1, big),
                custom(S1, "new name"),
                ai(S1, "New title"),
                prompt(S1, "new prompt"),
                filler(S1, 1000),
            ],
        );
        assert_eq!(s.title.as_deref(), Some("new name"));
        assert_eq!(s.last_prompt.as_deref(), Some("new prompt"));
        // Not at the end: found at the start, as Claude finds it. A name
        // at the start still wins over a title at the end.
        let s = session(
            tmp.path(),
            S1,
            &[
                custom(S1, "named early"),
                prompt(S1, "early prompt"),
                filler(S1, big),
                ai(S1, "Late title"),
            ],
        );
        assert_eq!(s.title.as_deref(), Some("named early"));
        assert_eq!(s.last_prompt.as_deref(), Some("early prompt"));
        // In the middle only: not read (Claude would not show it either).
        let s = session(
            tmp.path(),
            S1,
            &[
                filler(S1, big),
                ai(S1, "Lost in the middle"),
                filler(S1, big),
            ],
        );
        assert_eq!(s.title, None);
    }

    #[test]
    fn a_window_starting_inside_a_line_skips_it() {
        let tmp = tempfile::tempdir().unwrap();
        // A line ending in what reads as a title line, the window starting
        // right where it does: it is no line of its own.
        let inner = ai(S1, "Not a title");
        let junk = format!("{}{inner}", "y".repeat(100));
        // Lines after it fill the window up from there.
        let overhead = filler(S1, 0).len();
        let rest = filler(S1, WINDOW as usize - inner.len() - 2 - overhead);
        let lines = [ai(S1, "Real"), junk, rest];
        let file = tmp.path().join(format!("{S1}.jsonl"));
        write(&file, &lines);
        let len = std::fs::metadata(&file).unwrap().len();
        let start = (len - WINDOW) as usize;
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            bytes[start..].starts_with(inner.as_bytes()),
            "window misplaced"
        );
        assert_eq!(
            claude_session(&file).unwrap().title.as_deref(),
            Some("Real")
        );
    }

    #[test]
    fn lines_of_another_session_and_broken_lines_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session(
            tmp.path(),
            S1,
            &[
                ai(S1, "Mine"),
                ai("other-session", "Not mine"),
                "{\"type\":\"ai-title\",\"aiTitle\":".into(),
                "{\"type\":\"ai-title\",\"aiTitle\":42}".into(),
                "not json -title".into(),
            ],
        );
        assert_eq!(s.title.as_deref(), Some("Mine"));
        assert_eq!(claude_session(&tmp.path().join("missing.jsonl")), None);
        std::fs::create_dir(tmp.path().join("dir.jsonl")).unwrap();
        assert_eq!(claude_session(&tmp.path().join("dir.jsonl")), None);
    }

    #[test]
    fn text_is_made_one_short_line() {
        assert_eq!(
            clean("  a\n\tb\u{1b}[31m  c\u{7} "),
            Some("a b [31m c".into())
        );
        assert_eq!(clean(" \n "), None);
        let long = "é".repeat(MAX_TEXT + 5);
        let cut = clean(&long).unwrap();
        assert_eq!(cut.chars().count(), MAX_TEXT + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(
            clean(&"é".repeat(MAX_TEXT)).unwrap().chars().count(),
            MAX_TEXT
        );
    }

    #[test]
    fn history_lists_a_directorys_sessions_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        let repo = Path::new("/src/api");
        let project = project_dir(claude, repo);
        let file = |id: &str| project.join(format!("{id}.jsonl"));
        write(
            &file("old"),
            &[prompt("old", "first try"), ai("old", "Old")],
        );
        write(&file("new"), &[prompt("new", "second try")]);
        write(&file("named"), &[custom("named", "mine")]);
        // Nobody typed in it: not listed.
        write(&file("blank"), &[filler("blank", 10)]);
        // Not sessions.
        write(&project.join("notes.txt"), &[ai("notes", "x")]);
        std::fs::create_dir_all(project.join("old").join("subagents")).unwrap();
        write(
            &project.join("old").join("subagents").join("agent-1.jsonl"),
            &[ai("old", "Subagent")],
        );
        // Another directory's.
        write(
            &project_dir(claude, Path::new("/src/web")).join("web.jsonl"),
            &[ai("web", "Web")],
        );
        set_mtime(&file("old"), 1_000);
        set_mtime(&file("new"), 3_000);
        set_mtime(&file("named"), 2_000);
        set_mtime(&file("blank"), 4_000);

        let history = claude_history(claude, repo);
        let ids: Vec<&str> = history.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "named", "old"]);
        assert_eq!(
            history[2],
            SessionInfo {
                id: "old".into(),
                title: Some("Old".into()),
                last_prompt: Some("first try".into()),
                modified_at: 1_000,
            }
        );
        assert!(super::claude_history(claude, Path::new("/src/none")).is_empty());
    }

    // --- pi ---

    const PI_ID: &str = "0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c";

    fn pi_header_line(id: &str, cwd: &Path) -> String {
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": id,
            "timestamp": "2024-12-03T14:00:00.000Z",
            "cwd": cwd,
        })
        .to_string()
    }

    fn pi_name(name: &str) -> String {
        serde_json::json!({"type": "session_info", "id": "n1", "parentId": null,
                           "timestamp": "2024-12-03T14:00:00.000Z", "name": name})
        .to_string()
    }

    fn pi_nameless() -> String {
        serde_json::json!({"type": "session_info", "id": "n2", "parentId": "n1",
                           "timestamp": "2024-12-03T14:00:00.000Z", "name": null})
        .to_string()
    }

    fn pi_user(text: &str) -> String {
        serde_json::json!({"type": "message", "id": "m1", "parentId": null,
                           "timestamp": "2024-12-03T14:00:01.000Z",
                           "message": {"role": "user", "content": text, "timestamp": 1}})
        .to_string()
    }

    fn pi_user_blocks(text: &str) -> String {
        serde_json::json!({"type": "message", "id": "m2", "parentId": "m1",
                           "timestamp": "2024-12-03T14:00:01.000Z",
                           "message": {"role": "user",
                                       "content": [{"type": "text", "text": text}],
                                       "timestamp": 1}})
        .to_string()
    }

    fn pi_assistant(text: &str) -> String {
        serde_json::json!({"type": "message", "id": "m3", "parentId": "m2",
                           "timestamp": "2024-12-03T14:00:02.000Z",
                           "message": {"role": "assistant",
                                       "content": [{"type": "text", "text": text}],
                                       "stopReason": "stop", "timestamp": 2}})
        .to_string()
    }

    /// A pi session file in `dir`, as pi names it.
    fn pi_file(dir: &Path, id: &str, lines: &[String]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let file = dir.join(format!("2024-12-03T14-25-00-000Z_{id}.jsonl"));
        write(&file, lines);
        file
    }

    #[test]
    fn pi_project_names_are_pis() {
        assert_eq!(
            pi_project_name(Path::new("/home/user/Workspace/tam")),
            "--home-user-Workspace-tam--"
        );
        assert_eq!(pi_project_name(Path::new("/")), "----");
        // A path with what pi makes a dash, and one it leaves alone.
        assert_eq!(pi_project_name(Path::new("/a b/c:d")), "--a b-c-d--");
        assert_eq!(pi_project_name(Path::new("/")), "----");
        assert_eq!(pi_project_name(Path::new("/")), "----");
    }

    #[test]
    fn pi_sessions_read_the_name_the_user_gave_and_the_last_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/src/app");
        let file = pi_file(
            tmp.path(),
            PI_ID,
            &[
                pi_header_line(PI_ID, cwd),
                pi_user("first question"),
                pi_assistant("an answer"),
                pi_name("pool leak"),
                pi_user_blocks("and the tests"),
            ],
        );
        let session = pi_session(&file).unwrap();
        assert_eq!(session.id, PI_ID);
        assert_eq!(session.title.as_deref(), Some("pool leak"));
        // The latest user message, blocks joined, one line.
        assert_eq!(session.last_prompt.as_deref(), Some("and the tests"));

        // A name taken back: pi writes an empty one.
        let file = pi_file(
            tmp.path(),
            "1a2b3c4d-0000-4000-8000-000000000002",
            &[
                pi_header_line("1a2b3c4d-0000-4000-8000-000000000002", cwd),
                pi_name("gone"),
                pi_nameless(),
            ],
        );
        assert_eq!(pi_session(&file).unwrap().title, None);
        // Nothing of it: no title, no prompt.
        let file = pi_file(
            tmp.path(),
            "1a2b3c4d-0000-4000-8000-000000000003",
            &[
                pi_header_line("1a2b3c4d-0000-4000-8000-000000000003", cwd),
                pi_assistant("only I spoke"),
            ],
        );
        let session = pi_session(&file).unwrap();
        assert_eq!((session.title, session.last_prompt), (None, None));
        // A file that is not a session's, and one that is not there.
        let other = tmp.path().join("other.jsonl");
        std::fs::write(&other, "{\"type\":\"message\"}\n").unwrap();
        assert_eq!(pi_session(&other), None);
        assert_eq!(pi_session(&tmp.path().join("missing.jsonl")), None);
    }

    #[test]
    fn pi_reads_a_name_from_the_start_when_the_end_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/src/app");
        // A name, then more than a window of turns.
        let mut lines = vec![
            pi_header_line(PI_ID, cwd),
            pi_name("named early"),
            pi_user("first"),
        ];
        for i in 0..3000 {
            lines.push(pi_assistant(&format!("line {i} {}", "x".repeat(80))));
        }
        let file = pi_file(tmp.path(), PI_ID, &lines);
        assert!(std::fs::metadata(&file).unwrap().len() > WINDOW);
        let session = pi_session(&file).unwrap();
        assert_eq!(session.title.as_deref(), Some("named early"));
        // The last prompt is nowhere in the end's window: the start's is kept.
        assert_eq!(session.last_prompt.as_deref(), Some("first"));
    }

    #[test]
    fn claude_sessions_are_found_in_any_project() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        assert!(!claude_has_session(claude, "s1"), "no projects dir");
        let project = claude.join("projects").join("-w-app-feat");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("s1.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(claude.join("projects").join("-w-other")).unwrap();

        assert!(claude_has_session(claude, "s1"));
        assert!(!claude_has_session(claude, "s2"));
        // Not a session id: never looked up as a path.
        assert!(!claude_has_session(claude, ""));
        assert!(!claude_has_session(claude, "../projects/-w-app-feat/s1"));
    }

    #[test]
    fn pi_sessions_are_found_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c";
        pi_file(tmp.path(), id, &[pi_header_line(id, Path::new("/src/app"))]);
        assert!(dir_holds_session(tmp.path(), id));
        assert!(!dir_holds_session(
            tmp.path(),
            "11111111-2222-3333-4444-555555555555"
        ));
        // Nothing that could leave the directory.
        for bad in ["", "..", "a/b", "0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c/../x"] {
            assert!(!dir_holds_session(tmp.path(), bad), "{bad}");
        }
        assert!(!dir_holds_session(&tmp.path().join("nope"), id));
    }

    #[test]
    fn pi_history_and_session_files_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/src/app");
        let project = tmp.path().join(pi_project_name(cwd));
        let named = pi_file(
            &project,
            "1a2b3c4d-0000-4000-8000-000000000001",
            &[
                pi_header_line("1a2b3c4d-0000-4000-8000-000000000001", cwd),
                pi_user("hello"),
                pi_name("pool"),
            ],
        );
        let asked = pi_file(
            &project,
            "1a2b3c4d-0000-4000-8000-000000000002",
            &[
                pi_header_line("1a2b3c4d-0000-4000-8000-000000000002", cwd),
                pi_user("just a question"),
            ],
        );
        // A session nobody typed in says nothing worth listing.
        pi_file(
            &project,
            "1a2b3c4d-0000-4000-8000-000000000003",
            &[
                pi_header_line("1a2b3c4d-0000-4000-8000-000000000003", cwd),
                pi_assistant("nothing asked"),
            ],
        );
        set_mtime(&named, 2_000);
        set_mtime(&asked, 3_000);

        let history = pi_history_of(&project, cwd);
        let ids: Vec<&str> = history.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "1a2b3c4d-0000-4000-8000-000000000002",
                "1a2b3c4d-0000-4000-8000-000000000001"
            ]
        );
        assert_eq!(history[0].last_prompt.as_deref(), Some("just a question"));
        assert_eq!(history[1].title.as_deref(), Some("pool"));

        // By id: the file, and nothing for an id no file has, nor for a path.
        let found =
            pi_session_file_of(&project, cwd, "1a2b3c4d-0000-4000-8000-000000000002").unwrap();
        assert_eq!(found, asked);
        assert_eq!(
            pi_session_file_of(&project, cwd, "11111111-2222-3333-4444-555555555555"),
            None
        );
        for bad in ["", "..", "a/b"] {
            assert_eq!(pi_session_file_of(&project, cwd, bad), None, "{bad}");
        }

        // The one directory every project is in: only its own sessions.
        let flat = tmp.path().join("flat");
        std::fs::create_dir_all(&flat).unwrap();
        let elsewhere = pi_file(
            &flat,
            "1a2b3c4d-0000-4000-8000-000000000004",
            &[
                pi_header_line(
                    "1a2b3c4d-0000-4000-8000-000000000004",
                    Path::new("/src/other"),
                ),
                pi_user("not this repository"),
            ],
        );
        let mine = pi_file(
            &flat,
            "1a2b3c4d-0000-4000-8000-000000000005",
            &[
                pi_header_line("1a2b3c4d-0000-4000-8000-000000000005", cwd),
                pi_user("this one"),
            ],
        );
        set_mtime(&elsewhere, 1_000);
        set_mtime(&mine, 2_000);
        let history = pi_history_of(&flat, cwd);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "1a2b3c4d-0000-4000-8000-000000000005");
        assert_eq!(
            pi_session_file_of(&flat, cwd, "1a2b3c4d-0000-4000-8000-000000000004"),
            None
        );
    }
}
