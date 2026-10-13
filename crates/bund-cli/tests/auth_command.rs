//! These subprocesses must never query, create or remove actual Keychain items.
//! Agent requests explicitly use env-only mode; login is tested only after its
//! non-terminal input check, and status/logout are exercised solely via --help.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("home")).unwrap();
        let config = root.path().join("config.toml");
        fs::write(&config, "").unwrap();
        let state = root.path().join("state");
        Self {
            root,
            config,
            state,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bund"));
        command
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config-home"))
            .env("XDG_STATE_HOME", self.root.path().join("state-home"))
            .current_dir(self.root.path())
            .arg("--no-keychain")
            .arg("--config")
            .arg(&self.config)
            .arg("--workspace")
            .arg(self.root.path())
            .arg("--state-dir")
            .arg(&self.state);
        command
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn login_rejects_piped_credentials_before_secret_entry_or_storage() {
    let fixture = Fixture::new();
    let fake_key = "fake-piped-key-that-must-not-be-echoed-or-stored";
    let mut child = fixture
        .command()
        .args(["auth", "login", "--provider", "openai"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Login may reject stdin before the parent writes. BrokenPipe is expected
    // in that race, and is not evidence of consuming or saving the fake input.
    if let Some(mut input) = child.stdin.take() {
        if let Err(error) = writeln!(input, "{fake_key}") {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let output = text(&output);
    if cfg!(target_os = "macos") {
        assert!(output.to_lowercase().contains("terminal"), "{output}");
    } else {
        assert!(output.contains("only on macOS"), "{output}");
    }
    assert!(!output.contains(fake_key));
    assert!(
        !fixture.state.exists(),
        "login must not initialize session storage"
    );
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), "");
    assert_eq!(
        fs::read_dir(fixture.root.path().join("home"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn env_only_run_without_credentials_explains_auth_login_before_http() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args([
            "--provider",
            "openai",
            "--model",
            "fixture-model",
            // No listener exists at this endpoint. Credential resolution must
            // fail first; no connection or transport error should be reported.
            "--endpoint",
            "http://127.0.0.1:1/v1",
            "run",
            "hello",
            "--output",
            "jsonl",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let output_text = text(&output);
    assert!(output_text.contains("auth login"), "{output_text}");
    assert!(output_text.contains("OPENAI_API_KEY"), "{output_text}");
    assert!(
        output_text.to_lowercase().contains("authentication"),
        "{output_text}"
    );
    assert!(!output_text.contains("provider 'openai' is not configured"));
    assert!(!output_text.to_lowercase().contains("connection refused"));
    assert!(!output_text.to_lowercase().contains("transport failed"));
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["kind"]["type"], "run_finished");
    assert_eq!(events.last().unwrap()["kind"]["data"]["status"], "failed");
}

#[test]
fn auth_and_config_help_remain_offline_with_unusable_runtime_paths() {
    let fixture = Fixture::new();
    let blocked = fixture.root.path().join("blocked");
    fs::write(&blocked, "ordinary file").unwrap();
    let commands: &[&[&str]] = &[
        &["auth", "--help"],
        &["auth", "login", "--help"],
        &["auth", "status", "--help"],
        &["auth", "logout", "--help"],
        &["config", "--help"],
        &["config", "show", "--help"],
    ];
    for args in commands {
        let output = Command::new(env!("CARGO_BIN_EXE_bund"))
            .env_clear()
            .env("HOME", fixture.root.path().join("home"))
            .arg("--no-keychain")
            .arg("--config")
            .arg(blocked.join("missing.toml"))
            .arg("--workspace")
            .arg(blocked.join("missing-workspace"))
            .arg("--state-dir")
            .arg(blocked.join("state"))
            .args(*args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "help failed for {args:?}: {}",
            text(&output)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    }
    assert!(!fixture.state.exists());
    assert_eq!(fs::read_to_string(blocked).unwrap(), "ordinary file");
}

#[cfg(target_os = "macos")]
#[test]
fn hidden_login_disables_echo_and_restores_terminal_after_invalid_input() {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::pty::openpty;
    use nix::sys::termios::{tcgetattr, LocalFlags};
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::thread;
    use std::time::{Duration, Instant};

    // The spaces make this credential invalid. auth::login_with validates it
    // before invoking the credential store, so this test can never save a key.
    const INVALID_SECRET: &str = "INVALID KEY MUST STAY HIDDEN";
    let fixture = Fixture::new();
    let pair = openpty(None, None).unwrap();
    let mut master = File::from(pair.master);
    let slave = File::from(pair.slave);
    let before = tcgetattr(&slave).unwrap();
    fcntl(&master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
    let mut bund = fixture.command();
    bund.args(["auth", "login", "--provider", "openai"]);
    // Keep a terminal session leader alive after the rejected login, allowing
    // macOS to expose the restored terminal attributes before PTY teardown.
    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .args([
            "-c",
            "\"$@\"; code=$?; printf '\\nAUTH_DONE\\n'; read done; exit \"$code\"",
            "bund-auth-pty-wrapper",
        ])
        .arg(bund.get_program())
        .args(bund.get_args())
        .current_dir(fixture.root.path())
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    for (key, value) in bund.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() < 0 || nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                unsafe {
                    nix::libc::kill(-(self.0.id() as i32), nix::libc::SIGKILL);
                }
                let _ = self.0.wait();
            }
        }
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut sent = false;
    let mut restored = false;
    loop {
        let mut buffer = [0; 4096];
        match master.read(&mut buffer) {
            Ok(n) => received.extend_from_slice(&buffer[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => {}
            Err(error) => panic!("auth PTY read failed: {error}"),
        }
        let output = String::from_utf8_lossy(&received);
        if !sent && output.contains("input hidden") {
            // The prompt may be printed just before termios is updated; wait
            // for the actual echo state rather than racing that transition.
            if !tcgetattr(&slave)
                .unwrap()
                .local_flags
                .contains(LocalFlags::ECHO)
            {
                writeln!(master, "{INVALID_SECRET}").unwrap();
                sent = true;
            }
        }
        if !restored && output.contains("AUTH_DONE") {
            assert!(sent, "hidden input prompt was never reached: {output}");
            assert!(
                !output.contains(INVALID_SECRET),
                "typed secret appeared in terminal output"
            );
            assert!(output.contains("nothing was saved"), "{output}");
            assert_eq!(
                before,
                tcgetattr(&slave).unwrap(),
                "all terminal settings must be restored"
            );
            master.write_all(b"done\n").unwrap();
            restored = true;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        assert!(Instant::now() < deadline, "auth PTY timed out: {output}");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(sent && restored);
    assert!(!fixture.state.exists());
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), "");
}
