//! A pi agent: tam writes the extension pi loads (`pi --extension <path>`)
//! and pi's events, in the payload shape of Claude's hooks, drive the
//! agent's state and session as Claude's do.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{AgentEnv, AgentState, Request, Response, ServerMessage, SessionChoice};
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
}

async fn start_daemon(sock: &Path, ledger: &Path) {
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200));
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

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// A fake pi: writes its arguments to `report`, then waits.
fn fake_pi(bin: &Path, report: &Path) {
    script(
        &bin.join("pi"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{r}.tmp'\nmv '{r}.tmp' '{r}'\nexec sleep 3600\n",
            r = report.display()
        ),
    );
}

async fn read_report(report: &Path) -> Vec<String> {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(report) {
            return text.lines().map(str::to_string).collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("pi did not start");
}

async fn agent_state(conn: &mut Conn, id: &str) -> AgentState {
    match conn.request(&Request::List).await {
        Response::Agents { agents } => {
            agents
                .into_iter()
                .find(|a| a.id == id)
                .unwrap_or_else(|| panic!("no agent {id}"))
                .state
        }
        other => panic!("expected Agents, got {other:?}"),
    }
}

#[tokio::test]
async fn a_pi_agent_loads_the_extension_tam_writes_and_reports_through_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    // The daemon writes the extension and the ledger here.
    std::env::set_var("XDG_DATA_HOME", &data);
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let ledger = data.join("ledger.jsonl");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let report = root.join("report");
    fake_pi(&bin, &report);
    let work = root.join("app--feat");
    std::fs::create_dir_all(&work).unwrap();

    start_daemon(&sock, &ledger).await;
    let mut cmd = Conn::connect(&sock).await;
    let member = match cmd
        .request(&Request::CreateTask {
            name: "feat".into(),
            members: vec![tam_proto::NewMember {
                repo_root: PathBuf::from("/src/app"),
                branch: "feat".into(),
                dir: work.clone(),
            }],
        })
        .await
    {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };

    // pi only on the caller's PATH, as a client sends it.
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let env = AgentEnv(BTreeMap::from([("PATH".to_string(), path)]));
    match cmd
        .request(&Request::RunAgent {
            member: member.clone(),
            provider: Some("pi".into()),
            session: SessionChoice::New,
            args: vec!["--model".into(), "x".into()],
            prompt: Some("-fix the thing".into()),
            env: env.clone(),
            wrapper: vec![],
        })
        .await
    {
        Response::Running { id, resumed } => {
            assert_eq!(id, member);
            assert_eq!(resumed, None);
        }
        other => panic!("expected Running, got {other:?}"),
    }

    // What the daemon wrote, and how pi was started: the extension first, a
    // prompt that begins with `-` after `--`.
    let extension = data.join("tam").join("agents").join("pi.js");
    let js = std::fs::read_to_string(&extension).expect("the extension was not written");
    assert!(js.contains("\"hook-notify\"") && js.contains("\"inbox-wait\""));
    assert!(!js.contains("__TAM_PROGRAM__"), "{js}");
    assert_eq!(
        read_report(&report).await,
        [
            "--extension".to_string(),
            extension.display().to_string(),
            "--model".to_string(),
            "x".to_string(),
            "--".to_string(),
            "-fix the thing".to_string(),
        ]
    );

    // The events the extension forwards: Claude's payloads, read by the
    // daemon whatever the agent.
    match cmd
        .request(&Request::HookEvent {
            agent_id: member.clone(),
            event: "SessionStart".into(),
            payload: Some(serde_json::json!({
                "session_id": "0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c",
                "source": "startup",
            })),
        })
        .await
    {
        Response::HookContext { context } => {
            assert!(context.contains("feat"), "{context}");
        }
        other => panic!("expected HookContext, got {other:?}"),
    }
    assert_eq!(
        agent_state(&mut cmd, &member).await,
        AgentState::Working,
        "a session start leaves a working agent working"
    );
    match cmd
        .request(&Request::HookEvent {
            agent_id: member.clone(),
            event: "UserPromptSubmit".into(),
            payload: Some(serde_json::json!({
                "session_id": "0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c",
                "prompt": "fix the thing",
            })),
        })
        .await
    {
        Response::Ok => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    match cmd
        .request(&Request::HookEvent {
            agent_id: member.clone(),
            event: "Stop".into(),
            payload: Some(serde_json::json!({
                "session_id": "0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c",
                "last_assistant_message": "Fixed it.",
            })),
        })
        .await
    {
        Response::Ok => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(agent_state(&mut cmd, &member).await, AgentState::Ready);

    // The run is the member's, under pi, in the session the hooks named.
    match cmd
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await
    {
        Response::Task { runs, .. } => {
            let run = runs.last().expect("no run");
            assert_eq!(run.provider, "pi", "{run:?}");
            assert_eq!(
                run.session_id.as_deref(),
                Some("0f9c1b7e-3a2d-4c5e-8b6a-1d2e3f4a5b6c")
            );
        }
        other => panic!("expected Task, got {other:?}"),
    }

    cmd.request(&Request::Kill { id: member }).await;
    cmd.request(&Request::Shutdown).await;
}
