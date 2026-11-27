//! A chat of pi's, through a daemon: the repository's history is pi's
//! session files, a chat made from one takes its name and last prompt, its
//! agent resumes that session, and it is told apart from a Claude chat.
//!
//! The session files are written here by hand, under a
//! `PI_CODING_AGENT_DIR` of the test's: kept to a single test, since it sets
//! process environment (with `HOME`, `XDG_*` and `CLAUDE_CONFIG_DIR`: no
//! real file is read).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tam_daemon::daemon::Daemon;
use tam_daemon::sessions;
use tam_proto::{ChatInfo, Request, Response, ServerMessage, SessionInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Conn {
    async fn connect(sock: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line).await {
                Ok(0) | Err(_) => panic!("daemon closed the connection"),
                Ok(_) => {
                    if let ServerMessage::Response(resp) =
                        serde_json::from_str(line.trim()).unwrap()
                    {
                        return resp;
                    }
                }
            }
        }
    }

    async fn chat(&mut self, request: &Request) -> ChatInfo {
        match self.request(request).await {
            Response::Chat { chat, .. } => *chat,
            other => panic!("expected Chat for {request:?}, got {other:?}"),
        }
    }

    async fn refused(&mut self, request: &Request) -> String {
        match self.request(request).await {
            Response::Error { message } => message,
            other => panic!("expected Error for {request:?}, got {other:?}"),
        }
    }

    async fn chats(&mut self) -> Vec<ChatInfo> {
        match self.request(&Request::ListChats).await {
            Response::Chats { chats } => chats,
            other => panic!("expected Chats, got {other:?}"),
        }
    }

    async fn history(&mut self, dir: &Path, provider: Option<&str>) -> Vec<SessionInfo> {
        let request = Request::ChatHistory {
            dir: dir.to_path_buf(),
            provider: provider.map(str::to_string),
        };
        match self.request(&request).await {
            Response::Sessions { sessions } => sessions,
            other => panic!("expected Sessions, got {other:?}"),
        }
    }
}

async fn start_daemon(sock: &Path, ledger: &Path) {
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200))
        // No need to wait for one: the files do not change under it.
        .chat_sessions_every(Duration::from_secs(3600));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    for _ in 0..60 {
        if UnixStream::connect(sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

fn git(dir: &Path, args: &[&str]) {
    let out = tam_worktree::git::command()
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository `name` under `root` with one commit; its main checkout,
/// resolved, as pi names its project.
fn repo(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("src").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
    dir.canonicalize().unwrap()
}

fn header(id: &str, cwd: &Path) -> String {
    serde_json::json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": "2024-12-03T14:00:00.000Z",
        "cwd": cwd,
    })
    .to_string()
}

fn named(name: &str) -> String {
    serde_json::json!({"type": "session_info", "name": name}).to_string()
}

fn asked(text: &str) -> String {
    serde_json::json!({
        "type": "message",
        "message": {"role": "user", "content": text, "timestamp": 1},
    })
    .to_string()
}

fn answered(text: &str) -> String {
    serde_json::json!({
        "type": "message",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stopReason": "stop",
        },
    })
    .to_string()
}

/// Write pi's session `session` of `dir`, last written at `at` (Unix
/// seconds), under pi's directory `pi`.
fn session(pi: &Path, dir: &Path, session: &str, lines: &[String], at: u64) {
    let project = pi.join("sessions").join(sessions::pi_project_name(dir));
    std::fs::create_dir_all(&project).unwrap();
    let file = project.join(format!("2024-12-03T14-25-00-000Z_{session}.jsonl"));
    let mut f = File::create(&file).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(at))
        .unwrap();
}

fn ids(sessions: &[SessionInfo]) -> Vec<&str> {
    sessions.iter().map(|s| s.id.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pi_chat_lives_in_pis_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    // Resolved: pi names the project after the path it ran in.
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    let data = root.join("data");
    let pi = root.join("pi");
    std::fs::create_dir_all(&data).unwrap();
    std::env::set_var("HOME", &root);
    std::env::set_var("XDG_DATA_HOME", &data);
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("PI_CODING_AGENT_DIR", &pi);
    // Empty: a Claude chat would find no session at all.
    std::env::set_var("CLAUDE_CONFIG_DIR", root.join("claude"));
    let ledger = data.join("ledger.jsonl");

    let api = repo(&root, "api");
    let held = "1a2b3c4d-0000-4000-8000-000000000001";
    let free = "1a2b3c4d-0000-4000-8000-000000000002";
    session(
        &pi,
        &api,
        held,
        &[
            header(held, &api),
            asked("why is the pool leaking?"),
            answered("a cursor, probably"),
            named("fix the pool leak"),
        ],
        3_000,
    );
    session(
        &pi,
        &api,
        free,
        &[header(free, &api), asked("and the tests?")],
        2_000,
    );
    // A session nobody typed in says nothing worth listing.
    session(
        &pi,
        &api,
        "1a2b3c4d-0000-4000-8000-000000000003",
        &[
            header("1a2b3c4d-0000-4000-8000-000000000003", &api),
            answered("nothing asked"),
        ],
        1_000,
    );
    // Another repository's session, in the same one directory.
    let web = repo(&root, "web");
    session(
        &pi,
        &web,
        "1a2b3c4d-0000-4000-8000-000000000004",
        &[
            header("1a2b3c4d-0000-4000-8000-000000000004", &web),
            asked("not this repository"),
        ],
        1_000,
    );

    start_daemon(&sock, &ledger).await;
    let mut conn = Conn::connect(&sock).await;

    // The history is pi's, newest first; Claude has none here.
    let history = conn.history(&api, Some("pi")).await;
    assert_eq!(ids(&history), [held, free]);
    assert_eq!(history[0].title.as_deref(), Some("fix the pool leak"));
    assert_eq!(
        history[0].last_prompt.as_deref(),
        Some("why is the pool leaking?")
    );
    assert!(conn.history(&api, None).await.is_empty());

    // A chat made from one of them, run by pi.
    let chat = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: Some(held.into()),
            provider: Some("pi".into()),
        })
        .await;
    assert_eq!(chat.provider, "pi");
    assert_eq!(chat.session.as_deref(), Some(held));
    assert_eq!(chat.title.as_deref(), Some("fix the pool leak"));
    assert_eq!(
        chat.last_prompt.as_deref(),
        Some("why is the pool leaking?")
    );
    assert_eq!(chat.label(), "fix the pool leak");
    let chats = conn.chats().await;
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].provider, "pi");

    // Its session is held now; the other still can be made into a chat.
    assert_eq!(ids(&conn.history(&api, Some("pi")).await), [free]);

    // Its agent resumes it: `pi --session <id>`, from the caller's PATH.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let report = root.join("report");
    let script = bin.join("pi");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{r}.tmp'\nmv '{r}.tmp' '{r}'\nexec sleep 3600\n",
            r = report.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    match conn
        .request(&Request::RunAgent {
            member: chat.id.clone(),
            provider: Some("pi".into()),
            session: Default::default(),
            args: vec![],
            prompt: None,
            env: tam_proto::AgentEnv(std::collections::BTreeMap::from([(
                "PATH".to_string(),
                path,
            )])),
            wrapper: Vec::new(),
        })
        .await
    {
        Response::Running { resumed, .. } => assert_eq!(resumed.as_deref(), Some(held)),
        other => panic!("expected Running, got {other:?}"),
    }
    let mut argv = String::new();
    for _ in 0..100 {
        argv = std::fs::read_to_string(&report).unwrap_or_default();
        if !argv.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        argv.lines().any(|arg| arg == held),
        "pi was not resumed in its session: {argv:?}"
    );
    assert!(matches!(
        conn.request(&Request::Kill {
            id: chat.id.clone()
        })
        .await,
        Response::Ok
    ));

    // Closed, its session is free again, and a chat can be made from it.
    let closed = conn
        .chat(&Request::CloseChat {
            chat: chat.id.clone(),
        })
        .await;
    assert_eq!(closed.provider, "pi");
    assert_eq!(ids(&conn.history(&api, Some("pi")).await), [held, free]);
    let again = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: Some("second".into()),
            session: Some(free.into()),
            provider: Some("pi".into()),
        })
        .await;
    assert_eq!(again.session.as_deref(), Some(free));

    // Neither agent takes the other's sessions.
    let claude = conn
        .refused(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: Some(held.into()),
            provider: None,
        })
        .await;
    assert!(claude.contains("claude has no session"), "{claude}");
    let unknown = conn
        .refused(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: Some("11111111-2222-3333-4444-555555555555".into()),
            provider: Some("pi".into()),
        })
        .await;
    assert!(unknown.contains("pi has no session"), "{unknown}");

    conn.request(&Request::Shutdown).await;
}
