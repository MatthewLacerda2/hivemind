//! The launchd user agent (SPEC §2, §10).
//!
//! macOS only. On anything else these commands say so and exit cleanly rather
//! than pretending — a Linux user running `hivemind service install` should
//! learn what to do instead, not watch a plist be written where nothing will
//! read it.

use std::path::{Path, PathBuf};

use crate::colour::Paint as _;
use anyhow::{Context as _, Result};

/// The launchd label, and the plist's basename.
pub(crate) const LABEL: &str = "dev.hivemind.daemon";

/// The template the release ships, filled in at install time.
const TEMPLATE: &str = include_str!("../../../packaging/launchd/dev.hivemind.daemon.plist");

/// Where the agent's plist goes.
pub(crate) fn plist_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new()
        .context("could not work out your home directory; pass --home")?;
    Ok(dirs
        .home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Fill the template in for this machine.
///
/// The binary's path is baked in rather than resolved at load time: launchd
/// starts the agent with almost no environment, so `hivemind` on PATH is not
/// something it can be relied on to find.
pub(crate) fn render(binary: &Path, home: &Path) -> String {
    TEMPLATE
        .replace("{{BINARY}}", &binary.to_string_lossy())
        .replace("{{HOME}}", &home.to_string_lossy())
}

/// Is this a machine launchd runs on?
pub(crate) fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// What to say on a machine without launchd.
pub(crate) fn unsupported_note() -> String {
    format!(
        "launchd is macOS only. On Linux, run `hivemind daemon` under systemd \
         or your supervisor of choice; the label to use is `{LABEL}`."
    )
}

/// Run `launchctl` with these arguments, returning what it said.
fn launchctl(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .context("could not run launchctl")?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("launchctl {}: {}", args.join(" "), stderr.trim())
}

/// The `gui/<uid>` domain this user's agents live in.
fn domain() -> String {
    // `bootstrap`/`bootout` need the domain explicitly; `load`/`unload` are
    // deprecated and silently do the wrong thing under some session types.
    format!("gui/{}", user_id())
}

/// This process's real user id.
///
/// Read from `id -u` rather than libc so this crate keeps its `unsafe_code =
/// "deny"` lint. It runs once per service command, which is not a hot path.
fn user_id() -> String {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "501".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )
}

/// Write the plist and start the agent (SPEC §2 step 3).
pub(crate) fn install(home: Option<&Path>) -> Result<()> {
    if !supported() {
        println!("{}", unsupported_note());
        return Ok(());
    }

    let home_dir = crate::paths::home(home)?;
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("could not create {}", home_dir.display()))?;

    // The binary running right now, resolved through any symlink Homebrew
    // left, so the plist points at something that will still be there.
    let binary = std::env::current_exe().context("could not find this binary's path")?;
    let binary = std::fs::canonicalize(&binary).unwrap_or(binary);

    let user_home = directories::BaseDirs::new()
        .context("could not work out your home directory")?
        .home_dir()
        .to_path_buf();

    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    // Replacing a running agent: take the old one out first, or launchctl
    // refuses with "service already loaded" and the new plist is never read.
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);

    std::fs::write(&path, render(&binary, &user_home))
        .with_context(|| format!("could not write {}", path.display()))?;

    launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])
        .context("could not start the agent")?;

    println!("{} {}", "installed".green(), path.display());
    println!("  it starts at login and restarts if it dies");
    Ok(())
}

/// Stop the agent and remove its plist.
pub(crate) fn uninstall() -> Result<()> {
    if !supported() {
        println!("{}", unsupported_note());
        return Ok(());
    }

    let path = plist_path()?;
    // Booting out something that is not loaded is not a failure: the caller
    // asked for it to be gone, and it is gone.
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);

    match std::fs::remove_file(&path) {
        Ok(()) => println!("{} {}", "removed".green(), path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("nothing installed");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not remove {}", path.display()));
        }
    }
    Ok(())
}

/// Stop and start the agent.
pub(crate) fn restart() -> Result<()> {
    if !supported() {
        println!("{}", unsupported_note());
        return Ok(());
    }

    let path = plist_path()?;
    anyhow::ensure!(
        path.is_file(),
        "no agent installed — run `hivemind service install` first"
    );

    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])
        .context("could not start the agent")?;

    println!("{}", "restarted".green());
    Ok(())
}

/// Show the daemon's log.
pub(crate) fn logs(home: Option<&Path>, lines: usize, follow: bool) -> Result<()> {
    let home_dir = crate::paths::home(home)?;
    let log = home_dir.join("launchd.err.log");

    if !log.is_file() {
        println!(
            "no log at {} — the daemon may never have run under launchd",
            log.display()
        );
        println!("run `hivemind daemon` in a terminal to see it start");
        return Ok(());
    }

    // `tail` rather than reading it ourselves: it already handles following a
    // file that is rotated, and reimplementing that badly is not a feature.
    let mut command = std::process::Command::new("tail");
    command.arg("-n").arg(lines.to_string());
    if follow {
        command.arg("-f");
    }
    command.arg(&log);

    let status = command.status().context("could not run tail")?;
    anyhow::ensure!(status.success(), "tail exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_has_both_placeholders_filled_in() {
        // A plist with `{{BINARY}}` still in it loads and then fails to start,
        // with launchd's reason buried in the system log.
        let rendered = render(
            Path::new("/opt/homebrew/bin/hivemind"),
            Path::new("/Users/someone"),
        );

        assert!(!rendered.contains("{{"), "nothing left to substitute");
        assert!(rendered.contains("<string>/opt/homebrew/bin/hivemind</string>"));
        assert!(rendered.contains("/Users/someone/.hivemind/launchd.err.log"));
    }

    #[test]
    fn the_agent_restarts_when_it_dies_but_not_when_it_was_stopped() {
        // SPEC §2 asks for KeepAlive. Without `SuccessfulExit false`,
        // `service stop` would be immediately undone by launchd.
        let rendered = render(Path::new("/usr/local/bin/hivemind"), Path::new("/home"));

        assert!(rendered.contains("<key>KeepAlive</key>"));
        assert!(rendered.contains("<key>SuccessfulExit</key>"));
        assert!(rendered.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn the_label_matches_the_plist_it_is_written_into() {
        // launchctl addresses the agent by label; a mismatch means `bootout`
        // silently does nothing and `install` then fails as "already loaded".
        let rendered = render(Path::new("/usr/local/bin/hivemind"), Path::new("/home"));
        assert!(
            rendered.contains(&format!("<string>{LABEL}</string>")),
            "the template's label must be the one the code uses"
        );
    }

    #[test]
    fn a_machine_without_launchd_is_told_what_to_do_instead() {
        // Writing a plist where nothing reads it is worse than saying so.
        let note = unsupported_note();
        assert!(note.contains("macOS"), "{note}");
        assert!(note.contains(LABEL), "and name the label: {note}");
    }

    #[test]
    fn a_path_with_spaces_survives_being_put_in_the_plist() {
        // "/Users/Ana Paula/..." is an ordinary home directory.
        let rendered = render(
            Path::new("/Applications/My Tools/hivemind"),
            Path::new("/Users/Ana Paula"),
        );
        assert!(rendered.contains("<string>/Applications/My Tools/hivemind</string>"));
        assert!(rendered.contains("/Users/Ana Paula/.hivemind/launchd.err.log"));
    }
}
