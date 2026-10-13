#![cfg(unix)]

use bund_core::{ApprovalDecision, EventKind, RunStatus, SessionStore};
use nix::{
    fcntl::{fcntl, FcntlArg, OFlag},
    pty::{openpty, Winsize},
    sys::termios::{tcgetattr, Termios},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
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
const REASONING: &str = "REASONING_ONLY_WHEN_EXPANDED\nInspect the workspace before editing.";
const LATEST: &str = "NEW_OUTPUT_WHILE_BROWSING";
const ORANGE: vt100::Color = vt100::Color::Rgb(255, 184, 108);
const EXIT_CONFIRMATION: &str = "Press Ctrl+C again to exit";
const EXIT_HELP: &str = "To exit, press Ctrl+C twice within 1.5 seconds";
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
    finish_after(id, items, 1)
}

fn finish_after(id: &str, items: &[Value], started: usize) -> String {
    let mut output = String::new();
    for (index, item) in items.iter().enumerate() {
        if index >= started {
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
        let reasoning = json!({"type":"reasoning","id":"rs1",
            "summary":[{"type":"summary_text","text":REASONING}],
            "encrypted_content":"OPAQUE_REASONING_MUST_STAY_PRIVATE"});
        let first = start("r1", &item)
            + &sse(json!({"type":"response.output_item.added","output_index":1,"item":reasoning}))
            + &sse(
                json!({"type":"response.reasoning_summary_text.delta","output_index":1,
                "item_id":"rs1","summary_index":0,"delta":REASONING}),
            )
            + &delta("Streaming ready 界\n");
        let proposed = (0..40)
            .map(|i| format!("Proposed line {i:02}: must not be written\n"))
            .collect::<String>();
        let call = json!({"type":"function_call","id":"fc1","call_id":"call1","name":"apply_patch",
            "arguments":json!({"path":"created.txt","content":proposed}).to_string(),"status":"completed"});
        let hostile = HOSTILE.iter().map(|text| delta(text)).collect::<String>();
        let completed = finish_after("r1", &[item, reasoning, call], 2);
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
        let rows = (0..45)
            .map(|i| format!("Scroll row {i:02}\n"))
            .collect::<String>();
        let response = start("r3", &message("m3", ""))
            + &delta(&format!("{rows}Cancellation stream is active\n"));
        let later = delta(&format!("{LATEST}\n"));
        // Deliberately leave the response unfinished until the client cancels.
        header(&mut socket, response.len() + later.len() + 1000);
        socket.write_all(response.as_bytes()).unwrap();
        socket.flush().unwrap();
        gates.recv_timeout(WAIT).unwrap();
        socket.write_all(later.as_bytes()).unwrap();
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
    screen: vt100::Parser,
}

impl Pty {
    fn spawn(dir: &Path, endpoint: &str) -> Self {
        Self::spawn_session(dir, endpoint, None)
    }

    fn spawn_session(dir: &Path, endpoint: &str, resume: Option<&str>) -> Self {
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
                "printf 'PREEXISTING_SCROLLBACK\\n'; /bin/sh -c 'printf \"%s\\n\" \"$$\" > \"$BUND_TEST_PID_FILE\"; exec \"$@\"' bund-pid-wrapper \"$@\"; code=$?; printf '\\nBUND_DONE\\n'; read done; exit \"$code\"",
                "bund-pty-wrapper",
            ])
            .arg(env!("CARGO_BIN_EXE_bund"))
            .arg("--no-keychain")
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
            ])
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env("OPENAI_API_KEY", "fixture-only-key")
            .env("BUND_TEST_PID_FILE", dir.join("bund.pid"))
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        if let Some(id) = resume {
            command.args(["resume", id]);
        }
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
            screen: vt100::Parser::new(24, 90, 1000),
        }
    }

    fn send(&mut self, text: &str) {
        self.master.write_all(text.as_bytes()).unwrap();
    }

    fn wheel(&mut self, down: bool) {
        self.send(if down {
            "\x1b[<65;10;10M"
        } else {
            "\x1b[<64;10;10M"
        });
    }

    fn pump(&mut self) {
        let mut buffer = [0; 8192];
        match self.master.read(&mut buffer) {
            Ok(n) => {
                for byte in &buffer[..n] {
                    self.received.push(*byte);
                    self.screen.process(&[*byte]);
                    if self.received.ends_with(b"\x1b[6n") {
                        let (row, col) = self.screen.screen().cursor_position();
                        self.send(&format!("\x1b[{};{}R", row + 1, col + 1));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => {}
            Err(error) => panic!("PTY read: {error}"),
        }
    }

    fn wait_screen(&mut self, label: &str, condition: impl Fn(&vt100::Screen) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            if condition(self.screen.screen()) {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "bund exited while waiting for {label}: {}",
                self.screen.screen().contents()
            );
            assert!(
                Instant::now() < deadline,
                "PTY timed out waiting for {label}: {}",
                self.screen.screen().contents()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_visible(&mut self, text: &str) {
        self.wait_screen(text, |screen| screen.contents().contains(text));
    }

    fn assert_occupied(&self) {
        let screen = self.screen.screen();
        assert!(
            screen.alternate_screen(),
            "Bund must own the terminal while open"
        );
        assert!(
            !screen.contents().contains("PREEXISTING_SCROLLBACK"),
            "the prior shell must remain hidden until exit"
        );
        assert_ne!(
            screen.mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "mouse scrolling must be captured by Bund"
        );
        assert_eq!(
            screen.mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
        assert_eq!(
            screen.scrollback(),
            0,
            "wheel input must not move native shell scrollback"
        );
    }

    fn interrupt_once(&mut self) {
        self.send("\x03");
        self.wait_visible(EXIT_CONFIRMATION);
        self.assert_occupied();
    }

    fn wait_recorded_text(&mut self, dir: &Path, needle: &str) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = bund_store::SqliteStore::open(dir.join("state")).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            let found = runtime.block_on(async {
                let sessions = store.list().await.unwrap();
                let events = store.events_after(&sessions[0].id, 0, 4096).await.unwrap();
                events.iter().any(|event| {
                    // Accept either retained legacy progress or the additive
                    // response-scoped event without coupling to its Rust shape.
                    let kind = serde_json::to_value(&event.kind).unwrap();
                    kind.pointer("/data/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains(needle))
                })
            });
            if found {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "stream delta was not recorded: {needle}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result =
            unsafe { nix::libc::ioctl(self.master.as_raw_fd(), nix::libc::TIOCSWINSZ, &size) };
        assert_eq!(result, 0);
        self.screen.screen_mut().set_size(rows, cols);
    }

    fn wait_composer(&mut self, draft: Option<&str>, approval: bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            let screen = self.screen.screen();
            let (rows, cols) = screen.size();
            let contents = screen.contents();
            // The five-row query box touches the terminal bottom in fullscreen
            // mode. Assert emulator cells, including color, rather than bytes.
            let top = rows - 5;
            let bottom = rows - 1;
            let bordered = screen.cell(top, 0).unwrap().contents() == "╭"
                && screen.cell(bottom, 0).unwrap().contents() == "╰"
                && screen.cell(bottom, cols - 1).unwrap().contents() == "╯"
                && screen.cell(bottom, 0).unwrap().fgcolor() == vt100::Color::Idx(6);
            let editor = screen
                .rows(0, cols)
                .skip(top as usize)
                .take(5)
                .collect::<Vec<_>>()
                .join("\n");
            let draft_visible =
                draft.is_none_or(|text| text.lines().all(|line| editor.contains(line)));
            let top_info = screen.rows(0, cols).take(7).collect::<Vec<_>>().join("\n");
            let approval_visible = !approval
                || (top_info.contains("Approval required: apply_patch")
                    && top_info.contains("Ctrl+Y")
                    && top_info.contains("Ctrl+N")
                    && editor.contains("draft paused"));
            if screen.alternate_screen()
                && bordered
                && editor.contains("Ask bund")
                && draft_visible
                && approval_visible
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "bottom composer missing or draft hidden (approval={approval}): {contents}"
            );
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "bund exited unexpectedly: {contents}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(&mut self) {
        // Reset any confirmation left by cancellation, then send exactly two
        // adjacent presses without depending on scheduling within the 1.5s window.
        self.send("\x1b[C\x03\x03");
        self.wait_finished();
    }

    fn wait_finished(&mut self) {
        self.wait_visible("BUND_DONE");
        assert_eq!(
            self.before,
            tcgetattr(&self.slave).unwrap(),
            "terminal settings must be fully restored"
        );
        assert!(
            !self.screen.screen().alternate_screen(),
            "exit must restore the primary screen"
        );
        assert_eq!(
            self.screen.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "exit must disable mouse reporting"
        );
        assert_eq!(
            self.screen.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );
        assert!(
            self.screen
                .screen()
                .contents()
                .contains("PREEXISTING_SCROLLBACK"),
            "the original shell contents must return on exit"
        );
        assert!(
            !self.screen.screen().contents().contains("Ask bund"),
            "fullscreen composer must not remain on the restored shell"
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
fn only_consecutive_interrupts_exit_and_other_input_or_timeout_resets_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = Pty::spawn(dir.path(), "http://127.0.0.1:1/v1");
    terminal.wait_composer(None, false);
    terminal.wait_visible("Latest conversation");
    terminal.assert_occupied();

    terminal.interrupt_once(); // An empty composer no longer exits on one press.
    terminal.send("\x04");
    terminal.wait_visible(EXIT_HELP);
    terminal.assert_occupied();
    terminal.send("/exit\r");
    terminal.wait_composer(Some("/exit"), false);
    terminal.wait_visible(EXIT_HELP);
    terminal.assert_occupied();
    terminal.send(&"\x7f".repeat(5));
    terminal.wait_visible("Ask bund about this workspace");

    terminal.send(&format!("\x1b[200~{DRAFT}\x1b[201~"));
    terminal.wait_composer(Some(DRAFT), false);
    terminal.interrupt_once();
    terminal.wait_composer(Some(DRAFT), false);
    terminal.send("!"); // Editing between presses disarms the first press.
    let edited = format!("{DRAFT}!");
    terminal.wait_composer(Some(&edited), false);
    terminal.interrupt_once();
    terminal.wait_composer(Some(&edited), false);

    // Await the actual expiry redraw rather than racing a sleep with input.
    terminal.wait_screen("exit confirmation timeout", |screen| {
        !screen.contents().contains(EXIT_CONFIRMATION)
    });
    terminal.interrupt_once();
    terminal.wait_composer(Some(&edited), false);
    terminal.wheel(false); // A deliberate scroll also interrupts the sequence.
    terminal.wait_screen("scroll resets exit confirmation", |screen| {
        !screen.contents().contains(EXIT_CONFIRMATION)
    });
    terminal.interrupt_once();
    terminal.wait_composer(Some(&edited), false);
    terminal.finish();
}

#[test]
fn operating_system_interrupts_require_confirmation_and_restore_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = Pty::spawn(dir.path(), "http://127.0.0.1:1/v1");
    terminal.wait_composer(None, false);
    terminal.wait_visible("Latest conversation");
    let pid: nix::libc::pid_t = std::fs::read_to_string(dir.path().join("bund.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 1 && pid as u32 != terminal.child.id());
    let interrupt = || assert_eq!(unsafe { nix::libc::kill(pid, nix::libc::SIGINT) }, 0);
    interrupt();
    terminal.wait_visible(EXIT_CONFIRMATION);
    terminal.assert_occupied();
    terminal.send("preserved after signal");
    terminal.wait_composer(Some("preserved after signal"), false);
    interrupt(); // Input interrupted the prior confirmation sequence.
    terminal.wait_visible(EXIT_CONFIRMATION);
    terminal.wait_composer(Some("preserved after signal"), false);
    terminal.assert_occupied();
    interrupt(); // A second real signal confirms exit, without targeting the wrapper.
    terminal.wait_finished();
}

#[test]
fn first_interrupt_cancels_pending_approval_without_closing_or_losing_draft() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (advance, gate) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, _) = accept_request(&listener);
        let item = message("approval-message", "Preparing approval\n");
        let first = start("approval-response", &item) + &delta("Preparing approval\n");
        let call = json!({"type":"function_call","id":"approval-call","call_id":"approval-call",
            "name":"apply_patch","arguments":json!({"path":"created.txt","content":"unapproved"}).to_string(),
            "status":"completed"});
        let completed = finish("approval-response", &[item, call]);
        header(&mut socket, first.len() + completed.len());
        socket.write_all(first.as_bytes()).unwrap();
        socket.flush().unwrap();
        gate.recv_timeout(WAIT).unwrap();
        socket.write_all(completed.as_bytes()).unwrap();
    });
    let mut terminal = Pty::spawn(dir.path(), &endpoint);
    terminal.wait_composer(None, false);
    terminal.send("prepare an edit\r");
    terminal.wait_visible("Preparing approval");
    terminal.send(&format!("\x1b[200~{DRAFT}\x1b[201~"));
    terminal.wait_composer(Some(DRAFT), false);
    advance.send(()).unwrap();
    terminal.wait_composer(Some(DRAFT), true);
    terminal.interrupt_once();
    terminal.wait_screen("approval cancelled while Bund remains open", |screen| {
        screen.contents().contains("Cancelled")
            && !screen.contents().contains("Approval required: apply_patch")
    });
    terminal.wait_composer(Some(DRAFT), false);
    terminal.assert_occupied();
    assert!(!dir.path().join("created.txt").exists());
    terminal.finish();
    worker.join().unwrap();
}

#[test]
fn unicode_draft_survives_stream_approval_resize_and_cancel_with_inert_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let mut terminal = Pty::spawn(dir.path(), &fixture.endpoint);
    terminal.wait_composer(None, false);
    terminal.assert_occupied();
    assert!(!terminal
        .screen
        .screen()
        .contents()
        .contains("PREEXISTING_SCROLLBACK"));
    let header = terminal
        .screen
        .screen()
        .rows(0, 90)
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(header.contains("fixture-model"), "{header}");
    assert!(header.contains("Ctrl+O"), "{header}");
    terminal.send("begin fixture\r");
    terminal.wait_visible("Streaming ready");
    fixture.requests.recv_timeout(WAIT).unwrap();
    terminal.send(&format!("\x1b[200~{DRAFT}\x1b[201~"));
    terminal.wait_composer(Some(DRAFT), false);
    assert!(
        !String::from_utf8_lossy(&terminal.received).contains("REASONING_ONLY_WHEN_EXPANDED"),
        "hidden reasoning must never be painted before expansion"
    );
    terminal.send("\x0f"); // Ctrl+O expands inline details, preserving the draft.
    terminal.wait_visible("REASONING_ONLY_WHEN_EXPANDED");
    let screen = terminal.screen.screen();
    let (rows, cols) = screen.size();
    assert!(
        (4..rows - 5).any(|row| (0..cols).any(|col| {
            let cell = screen.cell(row, col).unwrap();
            cell.fgcolor() == ORANGE && matches!(cell.contents(), "╭" | "╰" | "│")
        })),
        "expanded activity needs an orange outline in the transcript"
    );
    terminal.wait_composer(Some(DRAFT), false);
    terminal.send("\x0f");
    terminal.wait_screen("collapsed activity", |screen| {
        let (rows, cols) = screen.size();
        !screen.contents().contains("REASONING_ONLY_WHEN_EXPANDED")
            && !(4..rows - 5)
                .any(|row| (0..cols).any(|col| screen.cell(row, col).unwrap().fgcolor() == ORANGE))
    });
    terminal.send("\r");
    terminal.wait_visible("is active.");
    fixture.advance.send(()).unwrap();
    terminal.wait_visible("visible-safe");
    fixture.advance.send(()).unwrap();
    terminal.wait_composer(Some(DRAFT), true);
    let review_before = terminal
        .screen
        .screen()
        .rows(0, 90)
        .skip(7)
        .take(12)
        .collect::<Vec<_>>();
    terminal.send("\x1b[6~"); // PageDown scrolls only the long approval preview.
    terminal.wait_screen("paged approval preview", |screen| {
        screen.rows(0, 90).skip(7).take(12).collect::<Vec<_>>() != review_before
    });
    terminal.wait_composer(Some(DRAFT), true);
    let review_before_wheel = terminal
        .screen
        .screen()
        .rows(0, 90)
        .skip(7)
        .take(12)
        .collect::<Vec<_>>();
    terminal.wheel(false);
    terminal.wait_screen("mouse wheel scrolls approval preview", |screen| {
        screen.rows(0, 90).skip(7).take(12).collect::<Vec<_>>() != review_before_wheel
    });
    terminal.assert_occupied();
    terminal.wait_composer(Some(DRAFT), true);
    terminal.send("\x0f"); // Activity toggle must remain usable during approval.
    terminal.wait_composer(Some(DRAFT), true);
    terminal.send("\x0f");
    terminal.resize(18, 62);
    terminal.wait_composer(Some(DRAFT), true);
    terminal.resize(30, 100);
    terminal.wait_composer(Some(DRAFT), true);
    terminal.send("\x0e"); // Ctrl+N denies without touching the composer.
    terminal.wait_visible("Denial acknowledged");
    terminal.wait_visible("Completed");
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
    terminal.wait_visible("Cancellation stream is active");
    let request = fixture.requests.recv_timeout(WAIT).unwrap();
    assert_eq!(
        request["input"].as_array().unwrap().last().unwrap()["content"],
        DRAFT
    );
    terminal.send("\x1b[5~"); // PageUp pauses following streamed output.
    terminal.wait_visible("Browsing history");
    let (row, anchor) = terminal
        .screen
        .screen()
        .rows(0, 100)
        .enumerate()
        .find(|(_, line)| line.contains("Scroll row"))
        .expect("paging should expose an older response row");
    fixture.advance.send(()).unwrap();
    terminal.wait_recorded_text(dir.path(), LATEST);
    terminal.send("\x1b[200~still drafting 世界\x1b[201~");
    terminal.wait_composer(Some("still drafting 世界"), false);
    assert_eq!(
        terminal.screen.screen().rows(0, 100).nth(row).unwrap(),
        anchor,
        "new streaming output must preserve the scrolled message anchor"
    );
    assert!(
        !terminal.screen.screen().contents().contains(LATEST),
        "new output must not force the history view back to the bottom"
    );
    terminal.send("\x1b[6~");
    terminal.wait_visible("Browsing history");
    terminal.send("\x1b[1;5F"); // Ctrl+End resumes following the latest output.
    terminal.wait_screen("latest output after Ctrl+End", |screen| {
        screen.contents().contains(LATEST) && !screen.contents().contains("Browsing history")
    });
    terminal.wait_composer(Some("still drafting 世界"), false);
    terminal.interrupt_once(); // Cancel without closing or clearing the draft.
    terminal.wait_visible("Cancelled");
    terminal.wait_composer(Some("still drafting 世界"), false);
    terminal.assert_occupied();
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
        raw.contains("\x1b[?1049h"),
        "interactive UI must enter the alternate screen"
    );
    assert!(
        raw.contains("\x1b[?1049l"),
        "interactive UI must leave the alternate screen"
    );
    assert!(!raw.contains("OPAQUE_REASONING_MUST_STAY_PRIVATE"));
    assert!(!raw.contains("fixture-only-key"));
    let visible = bund_cli::output::clean(&raw);
    assert!(visible.contains("visible-safe"), "{visible}");
    assert!(visible.contains("世界"));
    assert!(visible.contains("e\u{301}"));
    assert!(visible.contains('🦀'));

    let session_id = tokio::runtime::Runtime::new().unwrap().block_on(async {
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
        sessions[0].id.clone()
    });

    // No fixture server remains: expanded history must come entirely from the
    // durable journal, without a provider call or opaque continuation display.
    let mut resumed = Pty::spawn_session(dir.path(), &fixture.endpoint, Some(&session_id));
    resumed.wait_composer(None, false);
    // The composer is available while history loads in the background. Paging
    // begins only after the saved transcript is ready, rather than racing it.
    resumed.wait_visible("Latest conversation");
    assert!(!String::from_utf8_lossy(&resumed.received).contains("REASONING_ONLY_WHEN_EXPANDED"));
    resumed.send("\x0f");
    resumed.send(&"\x1b[5~".repeat(20));
    resumed.wait_visible("REASONING_ONLY_WHEN_EXPANDED");
    resumed.wait_composer(None, false);
    assert!(
        !String::from_utf8_lossy(&resumed.received).contains("OPAQUE_REASONING_MUST_STAY_PRIVATE")
    );
    resumed.send("\x0f\x1b[1;5F");
    resumed.wait_screen("resumed compact conversation", |screen| {
        !screen.contents().contains("REASONING_ONLY_WHEN_EXPANDED")
            && !screen.contents().contains("Browsing history")
    });
    resumed.finish();
}

#[test]
fn saved_conversation_pages_across_windows_without_skipping_turns() {
    use bund_core::{
        Engine, EngineDependencies, Message, ModelProfile, ResponsePurpose, Role, SessionEvent,
        SessionOptions, ValidationStatus,
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let session_id = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = Arc::new(bund_store::SqliteStore::open(dir.path().join("state")).unwrap());
        let engine = Engine::new(EngineDependencies {
            providers: vec![],
            tools: vec![],
            store: store.clone(),
        })
        .unwrap();
        let session = engine
            .create_session(SessionOptions::new(
                dir.path().to_owned(),
                ModelProfile::new("openai", "fixture-model"),
            ))
            .await
            .unwrap();
        let id = session.snapshot().await.unwrap().id;
        let mut stored = store.load(&id).await.unwrap();
        let mut events = Vec::new();
        // Three windows at the CLI's 32-run limit. Batch one valid journal
        // transaction; no model, credential lookup, or tool execution is needed.
        for index in 0..70 {
            let prompt = format!("Saved prompt {index:02}");
            let answer = format!("Saved answer {index:02}");
            let response_id = format!("response-{index}");
            let kinds = [
                EventKind::RunStarted {
                    prompt: prompt.clone(),
                },
                EventKind::ResponseStarted {
                    id: response_id.clone(),
                    purpose: ResponsePurpose::Conversation,
                },
                EventKind::ResponseCompleted {
                    id: response_id.clone(),
                    purpose: ResponsePurpose::Conversation,
                    text: answer.clone(),
                    reasoning: vec![format!("Saved reasoning {index:02}")],
                    tool_calls: vec![],
                },
                EventKind::RunOutput {
                    response_id: Some(response_id),
                    text: answer.clone(),
                },
                EventKind::RunFinished {
                    status: RunStatus::Completed,
                    message: String::new(),
                    validation: ValidationStatus::NotRun,
                },
            ];
            for kind in kinds {
                stored.cursor += 1;
                events.push(SessionEvent {
                    version: 1,
                    session_id: id.clone(),
                    sequence: stored.cursor,
                    run_id: Some(format!("run-{index}")),
                    kind,
                });
            }
            stored.messages.push(Message::text(Role::User, prompt));
            stored.messages.push(Message::text(Role::Assistant, answer));
        }
        stored.status = RunStatus::Completed;
        stored.run_id = None;
        store.commit(&stored, &events).await.unwrap();
        id
    });

    // There is intentionally no listener at this endpoint. Navigation only
    // reads saved session data, with Keychain access disabled by the harness.
    let mut terminal = Pty::spawn_session(dir.path(), "http://127.0.0.1:1/v1", Some(&session_id));
    terminal.wait_composer(None, false);
    terminal.wait_visible("Latest conversation");
    terminal.wait_visible("Saved answer 69");
    terminal.send("\x1b[200~draft survives history paging\x1b[201~");
    terminal.wait_composer(Some("draft survives history paging"), false);

    fn answers(screen: &vt100::Screen) -> BTreeSet<usize> {
        screen
            .contents()
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("Saved answer ")
                    .and_then(|index| index.parse().ok())
            })
            .collect()
    }
    fn body(screen: &vt100::Screen) -> Vec<String> {
        let (rows, cols) = screen.size();
        screen
            .rows(0, cols)
            .skip(4)
            .take(rows.saturating_sub(9) as usize)
            .collect()
    }
    fn visit(terminal: &mut Pty, key: &str, target: usize) -> BTreeSet<usize> {
        let deadline = Instant::now() + WAIT;
        let mut seen = answers(terminal.screen.screen());
        while !seen.contains(&target) {
            assert!(
                Instant::now() < deadline,
                "history paging did not reach {target}: {seen:?}"
            );
            let before = body(terminal.screen.screen());
            terminal.send(key);
            terminal.wait_screen("adjacent saved-history page", |screen| {
                let text = screen.contents();
                !text.contains("Loading history")
                    && !text.contains("History is loading")
                    && body(screen) != before
            });
            seen.extend(answers(terminal.screen.screen()));
        }
        seen
    }

    let before_wheel = body(terminal.screen.screen());
    terminal.wheel(false);
    terminal.wait_screen("mouse wheel scrolls Bund history", |screen| {
        screen.contents().contains("Browsing history") && body(screen) != before_wheel
    });
    terminal.assert_occupied();
    terminal.wait_composer(Some("draft survives history paging"), false);
    let before_wheel = body(terminal.screen.screen());
    terminal.wheel(true);
    terminal.wait_screen("mouse wheel scrolls forward", |screen| {
        body(screen) != before_wheel
    });
    terminal.assert_occupied();
    terminal.send("\x1b[1;5F");
    terminal.wait_screen("latest after mouse wheel", |screen| {
        screen.contents().contains("Saved answer 69")
            && screen.contents().contains("Latest conversation")
            && !screen.contents().contains("Browsing history")
    });

    let expected: BTreeSet<_> = (0..70).collect();
    assert_eq!(
        visit(&mut terminal, "\x1b[5~", 0),
        expected,
        "paging backward must expose every saved answer"
    );
    assert_eq!(
        visit(&mut terminal, "\x1b[6~", 69),
        expected,
        "paging forward must not skip the first run of an adjacent window"
    );
    terminal.wait_composer(Some("draft survives history paging"), false);
    terminal.send("\x1b[1;5F");
    terminal.wait_screen("latest saved conversation", |screen| {
        screen.contents().contains("Saved answer 69")
            && screen.contents().contains("Latest conversation")
            && !screen.contents().contains("Browsing history")
    });
    terminal.interrupt_once();
    terminal.wait_composer(Some("draft survives history paging"), false);
    terminal.finish();
}
