use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn command(dir: &std::path::Path) -> Command {
    let config = dir.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bund"));
    command.args([
        "--no-keychain",
        "--workspace",
        dir.to_str().unwrap(),
        "--state-dir",
        dir.join("state").to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    for key in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "BUND_MODEL",
        "BUND_PROVIDER",
        "BUND_ENDPOINT",
    ] {
        command.env_remove(key);
    }
    command
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn response(call: bool) -> String {
    let item = if call {
        json!({"type":"function_call","id":"fc1","call_id":"call1","name":"apply_patch","arguments":"{\"path\":\"created.txt\",\"content\":\"must not be written\"}","status":"completed"})
    } else {
        json!({"type":"message","id":"msg1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Fixture response","annotations":[]}]})
    };
    let mut stream =
        event(json!({"type":"response.created","response":{"id":"resp1","status":"in_progress"}}));
    stream += &event(json!({"type":"response.output_item.added","output_index":0,"item":item}));
    if !call {
        stream += &event(
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg1","delta":"Fixture response"}),
        );
    }
    stream += &event(json!({"type":"response.output_item.done","output_index":0,"item":item}));
    stream += &event(
        json!({"type":"response.completed","response":{"id":"resp1","status":"completed","output":[item],"usage":{"input_tokens":12,"output_tokens":4}}}),
    );
    stream
}

fn fixture(body: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture was never called");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let read_deadline = Instant::now() + Duration::from_secs(30);
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let n = match socket.read(&mut buffer) {
                Ok(n) => n,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    assert!(
                        Instant::now() < read_deadline,
                        "fixture request body timed out: {error}"
                    );
                    continue;
                }
                Err(error) => panic!("fixture read: {error}"),
            };
            assert_ne!(n, 0);
            request.extend_from_slice(&buffer[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let len: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse()
                    .unwrap();
                if request.len() >= end + 4 + len {
                    break;
                }
            }
        }
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    (endpoint, thread)
}

#[test]
fn headless_jsonl_has_ordered_terminal_event_and_no_ui_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, server) = fixture(response(false));
    let output = command(dir.path())
        .args([
            "--model",
            "fixture-model",
            "--endpoint",
            &endpoint,
            "run",
            "hello",
            "--output",
            "jsonl",
        ])
        .env("OPENAI_API_KEY", "fixture-only-key")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events
        .windows(2)
        .all(|pair| pair[0]["sequence"].as_u64() < pair[1]["sequence"].as_u64()));
    assert_eq!(events.last().unwrap()["kind"]["type"], "run_finished");
    assert_eq!(
        events.last().unwrap()["kind"]["data"]["status"],
        "completed"
    );
}

#[test]
fn headless_approval_exits_three_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, server) = fixture(response(true));
    let output = command(dir.path())
        .args([
            "--model",
            "fixture-model",
            "--endpoint",
            &endpoint,
            "run",
            "create a file",
            "--output",
            "jsonl",
        ])
        .env("OPENAI_API_KEY", "fixture-only-key")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("created.txt").exists());
    assert!(String::from_utf8_lossy(&output.stdout).contains("approval_required"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-only-key"));
}

#[test]
fn configured_state_directory_recovers_from_unusable_default_location() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("blocked-state-base");
    std::fs::write(&blocked, "not a directory").unwrap();
    let state = dir.path().join("Application Support/bund");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        toml::to_string(&json!({ "state_dir": state })).unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bund"))
        .arg("--no-keychain")
        .args(["--workspace", dir.path().to_str().unwrap()])
        .arg("--config")
        .arg(config)
        .args(["sessions", "--json"])
        .env("XDG_STATE_HOME", blocked)
        .env_remove("BUND_PROVIDER")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([])
    );
    assert!(state.join("sessions.sqlite3").is_file());
}

#[test]
fn storage_startup_failure_names_path_and_explains_override() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::write(&state, "not a directory").unwrap();
    let output = command(dir.path()).arg("sessions").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(state.to_str().unwrap()), "{error}");
    assert!(error.contains("--state-dir PATH"), "{error}");
    assert!(
        error.contains("state_dir in your user configuration"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(state).unwrap(), "not a directory");
}

#[test]
fn missing_model_explains_setup_without_key_and_sessions_are_offline() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(dir.path())
        .args(["run", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("BUND_MODEL"));
    let output = command(dir.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([])
    );
}

#[test]
fn provider_override_does_not_inherit_another_providers_config_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut run = command(dir.path());
    std::fs::write(
        dir.path().join("config.toml"),
        "[model]\nprovider='openai'\nmodel='old-model'\nendpoint='https://old.example/v1'\n",
    )
    .unwrap();
    let output = run
        .args([
            "--provider",
            "anthropic",
            "--model",
            "chosen-model",
            "run",
            "hello",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1)); // No key: the session still records its selected profile.
    let output = command(dir.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let sessions: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(sessions[0]["profile"]["provider"], "anthropic");
    assert_eq!(sessions[0]["profile"]["model"], "chosen-model");
    assert!(sessions[0]["profile"]["endpoint"].is_null());
}

#[cfg(unix)]
#[test]
fn fullscreen_pty_resumes_policy_and_restores_shell_after_tiny_resize() {
    use nix::{
        fcntl::{fcntl, FcntlArg, OFlag},
        pty::{openpty, Winsize},
        sys::termios::tcgetattr,
    };
    use std::{
        fs::File,
        os::{fd::AsRawFd, unix::process::CommandExt},
    };
    let dir = tempfile::tempdir().unwrap();
    let session_id = tokio::runtime::Runtime::new().unwrap().block_on(async {
        use bund_core::{Engine, EngineDependencies, ModelProfile, SessionOptions};
        let engine = Engine::new(EngineDependencies {
            providers: vec![],
            tools: bund_tools::built_in_tools(),
            store: std::sync::Arc::new(
                bund_store::SqliteStore::open(dir.path().join("state")).unwrap(),
            ),
        })
        .unwrap();
        let mut options =
            SessionOptions::new(dir.path().to_path_buf(), ModelProfile::new("openai", ""));
        options.policy.accept_edits = true;
        options.policy.allowed_commands = vec!["cargo test".into()];
        engine
            .create_session(options)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .id
    });
    let pair = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .unwrap();
    let mut master = File::from(pair.master);
    let slave = File::from(pair.slave);
    let before = tcgetattr(&slave).unwrap();
    fcntl(&master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
    let mut bund = command(dir.path());
    bund.args(["resume", &session_id]);
    // Keep the terminal's session leader alive after bund exits, so macOS does
    // not revoke the slave before we verify that raw mode was restored.
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "printf 'SHELL_BEFORE_BUND\\n'; \"$@\"; code=$?; printf '\\nBUND_DONE\\n'; read done; exit \"$code\"",
        "bund-pty-wrapper",
    ]);
    command.arg(bund.get_program()).args(bund.get_args());
    for (key, value) in bund.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    command
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("NO_COLOR")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut received = Vec::new();
    let mut screen = vt100::Parser::new(24, 160, 1000);
    let mut cursor_scan = 0;
    let mut stage = 0;
    let mut restored = false;
    loop {
        let mut buffer = [0; 8192];
        match master.read(&mut buffer) {
            Ok(n) => {
                received.extend_from_slice(&buffer[..n]);
                screen.process(&buffer[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
        while cursor_scan + 4 <= received.len() {
            if &received[cursor_scan..cursor_scan + 4] == b"\x1b[6n" {
                master.write_all(b"\x1b[1;1R").unwrap();
                cursor_scan += 4;
            } else {
                cursor_scan += 1;
            }
        }
        let contents = screen.screen().contents();
        if stage == 0 && contents.contains("Ask bund") {
            assert!(screen.screen().alternate_screen());
            let header = screen
                .screen()
                .rows(0, 160)
                .take(4)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(header.contains("edits auto"), "{header}");
            assert!(
                header.contains("shell reviewed (1 exact grants)"),
                "{header}"
            );
            master.write_all(b"/help\r").unwrap();
            stage = 1;
        }
        if stage == 1 && contents.contains("cannot be undone") {
            master.write_all(b"\x1b[200~draft\nkept\x1b[201~").unwrap();
            stage = 2;
        }
        if stage == 2 && contents.contains("draft") && contents.contains("kept") {
            let size = Winsize {
                ws_row: 8,
                ws_col: 24,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                assert_eq!(
                    nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, &size),
                    0
                );
            }
            screen.screen_mut().set_size(8, 24);
            stage = 3;
        }
        if stage == 3
            && screen.screen().cell(7, 0).unwrap().contents() == "╰"
            && screen
                .screen()
                .rows(0, 24)
                .skip(5)
                .any(|line| line.contains("kept"))
        {
            let size = Winsize {
                ws_row: 16,
                ws_col: 50,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                assert_eq!(
                    nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, &size),
                    0
                );
            }
            screen.screen_mut().set_size(16, 50);
            stage = 4;
        }
        if stage == 4
            && screen.screen().cell(15, 0).unwrap().contents() == "╰"
            && screen
                .screen()
                .rows(0, 50)
                .skip(11)
                .any(|line| line.contains("draft"))
            && screen
                .screen()
                .rows(0, 50)
                .skip(11)
                .any(|line| line.contains("kept"))
        {
            master.write_all(b"\x03").unwrap();
            stage = 5;
        }
        if stage == 5 && contents.contains("Press Ctrl+C again to exit") {
            assert!(screen.screen().alternate_screen());
            let editor = screen
                .screen()
                .rows(0, 50)
                .skip(11)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(editor.contains("draft") && editor.contains("kept"));
            master.write_all(b"\x03").unwrap();
            stage = 6;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{}", String::from_utf8_lossy(&received));
            break;
        }
        if !restored && String::from_utf8_lossy(&received).contains("BUND_DONE") {
            let after = tcgetattr(&slave).unwrap();
            assert_eq!(before, after, "terminal modes must be restored");
            assert!(!screen.screen().alternate_screen());
            assert_eq!(
                screen.screen().mouse_protocol_mode(),
                vt100::MouseProtocolMode::None
            );
            assert!(screen.screen().contents().contains("SHELL_BEFORE_BUND"));
            restored = true;
            master.write_all(b"done\n").unwrap();
        }
        if Instant::now() >= deadline {
            unsafe {
                nix::libc::kill(-(child.id() as i32), nix::libc::SIGKILL);
            }
            let _ = child.wait();
            panic!("PTY timed out: {}", String::from_utf8_lossy(&received));
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(stage == 6 && restored);
    assert!(
        received.windows(8).any(|w| w == b"\x1b[?1049h"),
        "must enter alternate screen"
    );
    assert!(
        received.windows(8).any(|w| w == b"\x1b[?1049l"),
        "must leave alternate screen"
    );
}
