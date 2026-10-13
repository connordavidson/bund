//! Explicit user-initiated clipboard writes; selected text is never a command argument.
use anyhow::{Context, Result};
use std::{io::ErrorKind, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

const COPY_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn copy(text: String) -> Result<()> {
    copy_with_commands(&text, &platform_commands()?, COPY_TIMEOUT).await
}

#[derive(Debug, PartialEq, Eq)]
struct ClipboardCommand {
    program: String,
    args: Vec<String>,
}

impl ClipboardCommand {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_commands() -> Result<Vec<ClipboardCommand>> {
    Ok(vec![ClipboardCommand::new("/usr/bin/pbcopy", &[])])
}

#[cfg(target_os = "linux")]
fn platform_commands() -> Result<Vec<ClipboardCommand>> {
    let available = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    linux_commands(available("WAYLAND_DISPLAY"), available("DISPLAY"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_commands() -> Result<Vec<ClipboardCommand>> {
    anyhow::bail!("Clipboard copying is supported on macOS and Linux.")
}

#[cfg(any(target_os = "linux", test))]
fn linux_commands(wayland: bool, x11: bool) -> Result<Vec<ClipboardCommand>> {
    let mut commands = Vec::new();
    if wayland {
        commands.push(ClipboardCommand::new(
            "wl-copy",
            &["--type", "text/plain;charset=utf-8"],
        ));
    }
    if x11 {
        commands.push(ClipboardCommand::new("xclip", &["-selection", "clipboard"]));
        commands.push(ClipboardCommand::new("xsel", &["--clipboard", "--input"]));
    }
    anyhow::ensure!(
        !commands.is_empty(),
        "Clipboard unavailable: no Wayland or X11 display is configured."
    );
    Ok(commands)
}

/// The command list is injectable for tests; production uses only the fixed list above.
async fn copy_with_commands(
    text: &str,
    commands: &[ClipboardCommand],
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, async {
        for candidate in commands {
            let mut child = match Command::new(&candidate.program)
                .args(&candidate.args)
                .env_remove("OPENAI_API_KEY")
                .env_remove("ANTHROPIC_API_KEY")
                .stdin(Stdio::piped())
                // A clipboard utility must not draw over the TUI or echo selected text.
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(child) => child,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Could not start {} for clipboard copying", candidate.program));
                }
            };
            let mut stdin = child.stdin.take().context("Clipboard input pipe unavailable")?;
            stdin
                .write_all(text.as_bytes())
                .await
                .with_context(|| format!("Could not send selection to {}", candidate.program))?;
            drop(stdin); // Clipboard tools finish consuming input only after EOF.
            let status = child
                .wait()
                .await
                .with_context(|| format!("Could not finish clipboard copying with {}", candidate.program))?;
            anyhow::ensure!(
                status.success(),
                "Clipboard copying with {} failed ({status}); check your desktop clipboard service.",
                candidate.program
            );
            return Ok(());
        }
        let names = commands
            .iter()
            .map(|command| command.program.as_str())
            .collect::<Vec<_>>()
            .join(" or ");
        anyhow::bail!("Clipboard command unavailable; install {names} for your desktop session.")
    })
    .await
    .context("Clipboard copying timed out; check your desktop clipboard service.")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_commands_match_the_available_display_and_use_explicit_clipboard() {
        let wayland = linux_commands(true, false).unwrap();
        assert_eq!(wayland.len(), 1);
        assert_eq!(wayland[0].program, "wl-copy");
        assert_eq!(wayland[0].args, ["--type", "text/plain;charset=utf-8"]);
        let x11 = linux_commands(false, true).unwrap();
        assert_eq!(
            x11[0],
            ClipboardCommand::new("xclip", &["-selection", "clipboard"])
        );
        assert_eq!(
            x11[1],
            ClipboardCommand::new("xsel", &["--clipboard", "--input"])
        );
        let both = linux_commands(true, true).unwrap();
        assert_eq!(both[0], wayland[0]);
        assert_eq!(both[1..], x11);
        assert!(linux_commands(false, false).is_err());
    }

    #[cfg(unix)]
    fn fake_command(script: &str, args: &[&str]) -> ClipboardCommand {
        // This controlled test fixture is the only shell invocation in this module.
        let mut command = ClipboardCommand::new("/bin/sh", &["-c", script, "clipboard-fixture"]);
        command.args.extend(args.iter().map(|arg| (*arg).into()));
        command
    }

    #[tokio::test]
    async fn missing_commands_report_how_to_enable_copying_without_selected_text() {
        let temp = tempfile::tempdir().unwrap();
        let commands = [ClipboardCommand::new(
            temp.path().join("missing-clipboard").to_str().unwrap(),
            &[],
        )];
        let error = copy_with_commands("private selection", &commands, COPY_TIMEOUT)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("install"));
        assert!(message.contains("missing-clipboard"));
        assert!(!message.contains("private selection"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn text_is_preserved_via_stdin_and_fallback_only_skips_missing_programs() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("received");
        let arg_count = temp.path().join("arg-count");
        let text = "Unicode 世界 e\u{301}\n$(unexecuted); 'quoted' --flags\n";
        let commands = [
            ClipboardCommand::new(temp.path().join("missing").to_str().unwrap(), &[]),
            fake_command(
                "printf '%s' \"$#\" > \"$2\"; exec /bin/cat > \"$1\"",
                &[output.to_str().unwrap(), arg_count.to_str().unwrap()],
            ),
        ];
        copy_with_commands(text, &commands, COPY_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(output).unwrap(), text);
        assert_eq!(std::fs::read_to_string(arg_count).unwrap(), "2");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_command_does_not_fallback_or_echo_selected_text_in_errors() {
        let temp = tempfile::tempdir().unwrap();
        let fallback = temp.path().join("fallback");
        let commands = [
            fake_command("/bin/cat >&2; exit 7", &[]),
            fake_command("/bin/cat > \"$1\"", &[fallback.to_str().unwrap()]),
        ];
        let error = copy_with_commands("private selection", &commands, COPY_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failed"));
        assert!(!format!("{error:#}").contains("private selection"));
        assert!(!fallback.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_denied_does_not_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("not-executable");
        std::fs::write(&blocked, "fixture").unwrap();
        let fallback = temp.path().join("fallback");
        let commands = [
            ClipboardCommand::new(blocked.to_str().unwrap(), &[]),
            fake_command("/bin/cat > \"$1\"", &[fallback.to_str().unwrap()]),
        ];
        let error = copy_with_commands("selection", &commands, COPY_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Could not start"));
        assert!(!fallback.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_bounds_blocked_input_and_kills_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("pid");
        let commands = [fake_command(
            "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 10",
            &[pid_file.to_str().unwrap()],
        )];
        let error = copy_with_commands(
            &"x".repeat(1_000_000),
            &commands,
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let pid =
            nix::unistd::Pid::from_raw(std::fs::read_to_string(pid_file).unwrap().parse().unwrap());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match nix::sys::signal::kill(pid, None) {
                    Err(nix::errno::Errno::ESRCH) => break,
                    Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => panic!("could not inspect fixture child: {error}"),
                }
            }
        })
        .await
        .expect("clipboard fixture should be killed and reaped after timeout");
    }
}
