//! Starting the receiver automatically at logon, on Windows.
//!
//! The registry's per-user `Run` key
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) rather than a Startup-folder
//! shortcut or a scheduled task: it needs no administrator rights, no COM to build a
//! `.lnk`, and no separate uninstall step beyond deleting one value. A scheduled task
//! would additionally be able to run before logon, which is not wanted here — there is
//! no audio session to render into until somebody is logged in.
//!
//! Command-line quoting lives here rather than in the platform-specific half, and is
//! tested on every platform, because getting it wrong is the failure mode: a path with
//! a space in it silently registers as two arguments and the entry does nothing at
//! logon, with no error anywhere.

use anyhow::Result;

/// The value name written under the `Run` key. Also what a user would look for in
/// Task Manager's Startup tab.
pub const ENTRY_NAME: &str = "micbridge";

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// What is currently registered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// False on platforms with no implementation, so a frontend can hide the option
    /// rather than offering something that always fails.
    pub supported: bool,
    pub enabled: bool,
    /// The exact command line registered, if any.
    pub command: Option<String>,
    /// An entry exists but does not point at this executable.
    ///
    /// Worth surfacing: the usual cause is that the binary was moved or rebuilt
    /// somewhere else, which leaves an entry that fails silently at logon.
    pub stale: bool,
}

/// Quotes one argument for a Windows command line.
///
/// Follows the rules `CommandLineToArgvW` actually implements, which are not the
/// obvious ones: backslashes are literal *except* immediately before a quote, where
/// they must be doubled. Getting this wrong turns `C:\Program Files\micbridge.exe` into two
/// arguments and the entry does nothing.
pub fn quote_arg(arg: &str) -> String {
    // Only quote when needed, so a simple command line stays readable in the
    // registry and in Task Manager. Note what does *not* appear in this test:
    // backslashes alone never require quoting — they are literal unless a quote
    // follows them, which only matters inside the quoted form below. An empty
    // argument does need quotes, or it vanishes entirely.
    let needs_quotes = arg.is_empty() || arg.contains([' ', '\t', '"']);
    if !needs_quotes {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut pending_backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => pending_backslashes += 1,
            '"' => {
                // Double the run of backslashes, then escape the quote itself.
                out.extend(std::iter::repeat_n('\\', pending_backslashes * 2 + 1));
                pending_backslashes = 0;
                out.push('"');
            }
            other => {
                out.extend(std::iter::repeat_n('\\', pending_backslashes));
                pending_backslashes = 0;
                out.push(other);
            }
        }
    }
    // Trailing backslashes precede the closing quote, so they double too.
    out.extend(std::iter::repeat_n('\\', pending_backslashes * 2));
    out.push('"');
    out
}

/// Builds the command line that would be registered: this executable plus `args`.
pub fn command_line(args: &[String]) -> Result<String> {
    let exe = std::env::current_exe()
        .map_err(|err| anyhow::anyhow!("could not determine this executable's path: {err}"))?;
    Ok(command_line_for(&exe.to_string_lossy(), args))
}

/// The pure form, so the quoting can be tested without a real executable.
pub fn command_line_for(exe: &str, args: &[String]) -> String {
    let mut line = quote_arg(exe);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

#[cfg(windows)]
mod platform {
    use super::{Status, ENTRY_NAME, RUN_KEY};
    use anyhow::{Context, Result};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    fn run_key(access: u32) -> Result<RegKey> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, access)
            .with_context(|| format!(r"opening HKCU\{RUN_KEY}"))
    }

    pub fn status() -> Result<Status> {
        let key = run_key(KEY_READ)?;
        let command: Option<String> = key.get_value(ENTRY_NAME).ok();

        // Compare against the running executable so a moved or rebuilt binary is
        // reported rather than left to fail silently at the next logon.
        let stale = match (&command, std::env::current_exe()) {
            (Some(command), Ok(exe)) => {
                let exe = exe.to_string_lossy().to_lowercase();
                !command.to_lowercase().contains(&exe)
            }
            _ => false,
        };

        Ok(Status { supported: true, enabled: command.is_some(), command, stale })
    }

    pub fn enable(command: &str) -> Result<()> {
        let key = run_key(KEY_WRITE)?;
        key.set_value(ENTRY_NAME, &command.to_string())
            .with_context(|| format!("writing the {ENTRY_NAME} autostart entry"))
    }

    pub fn disable() -> Result<()> {
        let key = run_key(KEY_WRITE)?;
        match key.delete_value(ENTRY_NAME) {
            Ok(()) => Ok(()),
            // Already absent is the desired end state, not a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => {
                Err(err).with_context(|| format!("removing the {ENTRY_NAME} autostart entry"))
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Status;
    use anyhow::{bail, Result};

    pub fn status() -> Result<Status> {
        Ok(Status::default())
    }

    pub fn enable(_command: &str) -> Result<()> {
        bail!("autostart is only implemented for Windows")
    }

    pub fn disable() -> Result<()> {
        bail!("autostart is only implemented for Windows")
    }
}

/// True where autostart can actually be configured.
pub fn supported() -> bool {
    cfg!(windows)
}

/// What is registered right now.
pub fn status() -> Result<Status> {
    platform::status()
}

/// Registers this executable, with `args`, to run at logon. Returns the command line
/// written, so a caller can show exactly what it did.
pub fn enable(args: &[String]) -> Result<String> {
    let command = command_line(args)?;
    platform::enable(&command)?;
    Ok(command)
}

/// Removes the entry. Succeeds when there was nothing to remove.
pub fn disable() -> Result<()> {
    platform::disable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_simple_argument_is_left_alone() {
        // Quoting everything would work but makes the registry value and the Task
        // Manager entry needlessly hard to read.
        assert_eq!(quote_arg("recv"), "recv");
        assert_eq!(quote_arg("--game-mic"), "--game-mic");
        assert_eq!(quote_arg("42100"), "42100");
    }

    #[test]
    fn spaces_are_quoted() {
        // The failure this exists to prevent: without quoting, a path with a space
        // registers as two arguments and the entry silently does nothing at logon.
        assert_eq!(quote_arg("CABLE Output"), r#""CABLE Output""#);
        assert_eq!(
            quote_arg(r"C:\Program Files\micbridge\micbridge.exe"),
            r#""C:\Program Files\micbridge\micbridge.exe""#
        );
    }

    #[test]
    fn a_path_without_spaces_keeps_its_backslashes_unescaped() {
        // Backslashes are literal except before a quote, so a plain path must not be
        // mangled into C:\\Tools\\micbridge.exe.
        assert_eq!(quote_arg(r"C:\Tools\micbridge.exe"), r"C:\Tools\micbridge.exe");
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn backslashes_before_a_quote_are_doubled() {
        // The rule CommandLineToArgvW actually implements, and the one everybody
        // gets wrong: a backslash run is literal unless a quote follows it.
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn trailing_backslashes_are_doubled_before_the_closing_quote() {
        // Otherwise the closing quote is escaped by the path's own trailing slash and
        // the rest of the command line is swallowed.
        assert_eq!(quote_arg(r"C:\Program Files\"), r#""C:\Program Files\\""#);
    }

    #[test]
    fn an_empty_argument_survives_as_an_empty_argument() {
        assert_eq!(quote_arg(""), r#""""#);
    }

    #[test]
    fn a_command_line_joins_the_executable_and_its_arguments() {
        let line = command_line_for(
            r"C:\Program Files\micbridge\micbridge.exe",
            &["recv".into(), "--game-mic".into(), "CABLE Output".into()],
        );
        assert_eq!(
            line,
            r#""C:\Program Files\micbridge\micbridge.exe" recv --game-mic "CABLE Output""#
        );
    }

    #[test]
    fn a_command_line_with_no_arguments_is_just_the_executable() {
        assert_eq!(
            command_line_for(r"C:\micbridge\micbridge-gui.exe", &[]),
            r"C:\micbridge\micbridge-gui.exe"
        );
    }

    #[test]
    fn unsupported_platforms_report_it_rather_than_pretending() {
        // A frontend hides the control instead of offering something that fails.
        let status = status().expect("status should not error anywhere");
        assert_eq!(status.supported, cfg!(windows));
        if !cfg!(windows) {
            assert!(!status.enabled);
            assert!(status.command.is_none());
            assert!(enable(&[]).is_err(), "should refuse rather than silently do nothing");
            assert!(disable().is_err());
        }
    }
}
