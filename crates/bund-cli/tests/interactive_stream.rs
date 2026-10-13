#![cfg(unix)]

use bund_core::{ApprovalDecision, EventKind, RunStatus, SessionStore};
use nix::{
    fcntl::{fcntl, FcntlArg, OFlag},
    pty::{openpty, Winsize},
    sys::termios::{tcgetattr, Termios},
};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(30);
const DRAFT: &str = "Continue 世界 e\u{301} 🦀\nkeep this draft";
const HOSTILE: [&str; 3] = [
    "harmless \u{1b}[",
    "999;999Hvisible-safe\u{1b}]52;c;INERT_CLIPBOARD_MARKER",
    "\u{7} intact\n",
];

fn sse(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn message(id: &str, text: &str) -> Value {
    json!({"type":"message","id":id,"role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn start(id: &str, item: &Value) -> String {
    sse(json!({"type":"response.created","response":{"id":id,"status":"in_progress"}}))
        + &sse(json!({"type":"response.output_item.added","output_index":0,"item":item}))
}

fn delta(text: &str) -> String {
    sse(json!({"type":"response.output_text.delta","output_index":0,"delta":text}))
}

fn finish(id: &str, items: &[Value]) -> String {
    let mut output = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            output +=
                &sse(json!({"type":"response.output_item.added","output_index":index,"item":item}));
        }
        output +=
            &sse(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    output
        + &sse(
            json!({"type":"response.completed","response":{"id":id,"status":"completed",
        "output":items,"usage":{"input_tokens":12,"output_tokens":4}}}),
        )
}

fn accept_request(listener: &TcpListener) -> (TcpStream, Value) {
    let deadline = Instant::now() + WAIT;
    let mut socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "fixture request timed out");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    };
    socket.set_nonblocking(false).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let n = socket.read(&mut buffer).unwrap();
        assert_ne!(n, 0, "fixture request closed early");
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
                let body = serde_json::from_slice(&request[end + 4..end + 4 + len]).unwrap();
                return (socket, body);
            }
        }
        assert!(
            request.len() < 1_048_576,
            "unexpectedly large fixture request"
        );
    }
}

fn header(socket: &mut TcpStream, length: usize) {
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
}

struct Fixture {
    endpoint: String,
    advance: Sender<()>,
    requests: Receiver<Value>,
    worker: thread::JoinHandle<()>,
}

fn fixture() -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (advance, gates) = mpsc::channel();
    let (requests_tx, requests) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, request) = accept_request(&listener);
        requests_tx.send(request).unwrap();
        let text = format!("Streaming ready 界\n{}", HOSTILE.concat());
        let item = message("m1", &text);
        let first = start("r1", &item) + &delta("Streaming ready 界\n");
        let call = json!({"type":"function_call","id":"fc1","call_id":"call1","name":"apply_patch",
            "arguments":"{\"path\":\"created.txt\",\"content\":\"must not be written\"}","status":"completed"});
        let hostile = HOSTILE.iter().map(|text| delta(text)).collect::<String>();
        let completed = finish("r1", &[item, call]);
        header(&mut socket, first.len() + hostile.len() + completed.len());
        socket.write_all(first.as_bytes()).unwrap();
        socket.flush().unwrap();
        // The terminal explicitly releases the remaining stream after its busy
        // submission has been rejected, so this does not depend on network speed.
        gates.recv_timeout(WAIT).unwrap();
        socket.write_all(hostile.as_bytes()).unwrap();
        socket.flush().unwrap();
        // Require evidence that the hostile text actually passed through the
        // live renderer; absence of its controls alone would be a vacuous check.
        gates.recv_timeout(WAIT).unwrap();
        socket.write_all(completed.as_bytes()).unwrap();
        drop(socket);

        let (mut socket, request) = accept_request(&listener);
        requests_tx.send(request).unwrap();
        let item = message("m2", "Denial acknowledged\n");
        let response =
            start("r2", &item) + &delta("Denial acknowledged\n") + &finish("r2", &[item]);
        header(&mut socket, response.len());
        socket.write_all(response.as_bytes()).unwrap();
        drop(socket);

        let (mut socket, request) = accept_request(&listener);
        requests_tx.send(request).unwrap();
        let response = start("r3", &message("m3", "")) + &delta("Cancellation stream is active\n");
        // Deliberately leave the response unfinished until the client cancels.
        header(&mut socket, response.len() + 1000);
        socket.write_all(response.as_bytes()).unwrap();
        socket.flush().unwrap();
        gates.recv_timeout(WAIT).unwrap();
    });
    Fixture {
        endpoint,
        advance,
        requests,
        worker,
    }
}

struct Pty {
    master: File,
    slave: File,
    child: Child,
    before: Termios,
    received: Vec<u8>,
    cursor_scan: usize,
}

impl Pty {
    fn spawn(dir: &Path, endpoint: &str) -> Self {
        let pair = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 90,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .unwrap();
        let master = File::from(pair.master);
        let slave = File::from(pair.slave);
        let before = tcgetattr(&slave).unwrap();
        fcntl(&master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, "").unwrap();
        // A live session leader keeps the macOS slave accessible for tcgetattr
        // after bund exits. The final read is released only after verification.
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "\"$@\"; code=$?; printf '\\nBUND_DONE\\n'; read done; exit \"$code\"",
                "bund-pty-wrapper",
            ])
            .arg(env!("CARGO_BIN_EXE_bund"))
            .arg("--workspace")
            .arg(dir)
            .arg("--state-dir")
            .arg(dir.join("state"))
            .arg("--config")
            .arg(config)
            .args([
                "--provider",
                "openai",
                "--model",
                "fixture-model",
                "--endpoint",
                endpoint,
                "begin fixture",
            ])
            .env("TERM", "xterm-256color")
            .env("OPENAI_API_KEY", "fixture-only-key")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        for key in [
            "ANTHROPIC_API_KEY",
            "BUND_PROVIDER",
            "BUND_MODEL",
            "BUND_ENDPOINT",
        ] {
            command.env_remove(key);
        }
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() < 0 || nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            master,
            slave,
            child,
            before,
            received: Vec::new(),
            cursor_scan: 0,
        }
    }

    fn send(&mut self, text: &str) {
        self.master.write_all(text.as_bytes()).unwrap();
    }

    fn pump(&mut self) {
        let mut buffer = [0; 8192];
        match self.master.read(&mut buffer) {
            Ok(n) => self.received.extend_from_slice(&buffer[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => {}
            Err(error) => panic!("PTY read: {error}"),
        }
        while self.cursor_scan + 4 <= self.received.len() {
            if &self.received[self.cursor_scan..self.cursor_scan + 4] == b"\x1b[6n" {
                self.send("\x1b[1;1R");
                self.cursor_scan += 4;
            } else {
                self.cursor_scan += 1;
            }
        }
    }

    fn wait_text(&mut self, after: usize, needle: &str) {
        let deadline = Instant::now() + WAIT;
        // Ratatui may skip blank cells with cursor moves; this capture is not a
        // terminal emulator. Exact draft preservation is checked in the actual
        // provider request below, independently of these progress markers.
        let marker: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
        loop {
            self.pump();
            let output = bund_cli::output::clean(&String::from_utf8_lossy(&self.received[after..]));
            let searchable: String = output.chars().filter(|c| !c.is_whitespace()).collect();
            if searchable.contains(&marker) {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "bund exited while waiting for {needle:?}: {output}"
            );
            assert!(
                Instant::now() < deadline,
                "PTY timed out waiting for {needle:?}: {output}; raw={:?}",
                String::from_utf8_lossy(&self.received)
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn resize(&mut self) {
        let size = Winsize {
            ws_row: 18,
            ws_col: 62,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result =
            unsafe { nix::libc::ioctl(self.master.as_raw_fd(), nix::libc::TIOCSWINSZ, &size) };
        assert_eq!(result, 0);
    }

    fn finish(&mut self) {
        self.send("/exit\r");
        self.wait_text(0, "BUND_DONE");
        assert_eq!(
            self.before,
            tcgetattr(&self.slave).unwrap(),
            "terminal settings must be fully restored"
        );
        self.send("done\n");
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "wrapper did not exit");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // On a failed assertion, also reap the bund child in this new process
            // group; killing only the shell wrapper would leave it behind.
            unsafe {
                nix::libc::kill(-(self.child.id() as i32), nix::libc::SIGKILL);
            }
            // Closing the PTY also releases macOS terminal teardown if a session
            // leader was stopped in exit while this test assertion unwound.
            let null = File::open("/dev/null").unwrap();
            self.master = null.try_clone().unwrap();
            self.slave = null;
            let _ = self.child.wait();
        }
    }
}

#[test]
fn unicode_draft_survives_stream_approval_resize_and_cancel_with_inert_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let mut terminal = Pty::spawn(dir.path(), &fixture.endpoint);
    terminal.wait_text(0, "Streaming ready");
    fixture.requests.recv_timeout(WAIT).unwrap();
    terminal.send(&format!("\x1b[200~{DRAFT}\x1b[201~"));
    terminal.wait_text(0, "keep this draft");
    terminal.send("\r");
    terminal.wait_text(0, "is active.");
    fixture.advance.send(()).unwrap();
    terminal.wait_text(0, "visible-safe");
    fixture.advance.send(()).unwrap();
    terminal.wait_text(0, "Approval required: apply_patch");
    terminal.wait_text(0, "Your draft is preserved.");
    let before_resize = terminal.received.len();
    terminal.resize();
    terminal.wait_text(before_resize, "Your draft is preserved.");
    let before_deny = terminal.received.len();
    terminal.send("\x0e"); // Ctrl+N denies without touching the composer.
    terminal.wait_text(before_deny, "Denial acknowledged");
    terminal.wait_text(before_deny, "Completed");
    let followup = fixture.requests.recv_timeout(WAIT).unwrap();
    assert!(followup["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "function_call_output"
            && item["output"]
                .as_str()
                .unwrap_or("")
                .contains("user denied")
    }));
    assert!(!dir.path().join("created.txt").exists());
    terminal.send("\r"); // Submit the original, still untouched multiline draft.
    terminal.wait_text(0, "Cancellation stream is active");
    let request = fixture.requests.recv_timeout(WAIT).unwrap();
    assert_eq!(
        request["input"].as_array().unwrap().last().unwrap()["content"],
        DRAFT
    );
    terminal.send("\x03"); // Cancel an actual unfinished provider stream.
    terminal.wait_text(0, "Cancelled");
    fixture.advance.send(()).unwrap();
    terminal.finish();
    fixture.worker.join().unwrap();

    let raw = String::from_utf8_lossy(&terminal.received);
    assert!(
        !raw.contains("\x1b[999;999H"),
        "model CSI must not reach the terminal"
    );
    assert!(
        !raw.contains("\x1b]52;"),
        "model OSC must not reach the terminal"
    );
    assert!(
        !raw.contains("INERT_CLIPBOARD_MARKER"),
        "OSC payload must be removed"
    );
    assert!(
        !raw.contains("\x1b[?1049h"),
        "native scrollback must remain active"
    );
    assert!(!raw.contains("fixture-only-key"));
    let visible = bund_cli::output::clean(&raw);
    assert!(visible.contains("visible-safe"), "{visible}");
    assert!(visible.contains("世界"));
    assert!(visible.contains("e\u{301}"));
    assert!(visible.contains('🦀'));

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = bund_store::SqliteStore::open(dir.path().join("state")).unwrap();
        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, RunStatus::Cancelled);
        let events = store.events_after(&sessions[0].id, 0, 512).await.unwrap();
        assert!(events.iter().any(|event| matches!(
            event.kind,
            EventKind::ApprovalResolved {
                decision: ApprovalDecision::Deny,
                ..
            }
        )));
        let prompts: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::RunStarted { prompt } => Some(prompt.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            prompts,
            ["begin fixture", DRAFT],
            "busy Enter must not enqueue or damage the draft"
        );
    });
}
