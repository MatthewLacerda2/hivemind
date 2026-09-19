//! Claude Code hooks and MCP registration (SPEC §9.2, §9.3).
//!
//! A Claude turn cannot be interrupted, so hivemind surfaces mail at turn
//! boundaries instead: `SessionStart` and `UserPromptSubmit` run
//! `hivemind hook check`, which prints one line if there is unread mail and
//! nothing otherwise.
//!
//! The same three hooks are how the daemon knows which sessions are open
//! (SPEC §9.3). `hook check` tells it which session this turn belongs to and
//! what it is working on; `SessionEnd` closes it.

use std::path::{Path, PathBuf};

pub(crate) mod check;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

/// The events hivemind hooks into (SPEC §9.3).
///
/// The first two surface unread mail at a turn boundary *and* register the
/// session; `SessionEnd` only closes it. Expiry is what makes the session
/// list true, so this last one is a courtesy that closes a session sooner
/// than the half hour — a terminal killed outright never sends it, and
/// nothing depends on it arriving.
const HOOK_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "SessionEnd"];
/// How hivemind's hook entries are recognised on the way back out.
const HOOK_COMMAND: &str = "hivemind hook check";

/// `~/.claude/settings.json`.
pub(crate) fn claude_settings_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("could not work out your home directory")?;
    Ok(dirs.home_dir().join(".claude").join("settings.json"))
}

/// Read a settings file, treating "not there" as "empty object".
fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON. hivemind will not overwrite it — \
                 fix or move it and try again.",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(anyhow::Error::new(e).context(format!("could not read {}", path.display()))),
    }
}

/// Write settings back, atomically.
fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    // Someone else's Claude configuration is not a file to half-write.
    let temp = path.with_extension("json.hivemind-tmp");
    let text = serde_json::to_string_pretty(settings).context("could not encode settings")?;
    std::fs::write(&temp, text.as_bytes())
        .with_context(|| format!("could not write {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("could not move {} into place", temp.display()))
}

/// Does this hook entry belong to hivemind?
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(HOOK_COMMAND))
            })
        })
}

/// Merge hivemind's hooks into a settings document, leaving everything else be.
///
/// Separate from the file handling so it can be tested against documents that
/// would be tedious to create on disk.
pub(crate) fn merge_hooks(settings: &mut Value) {
    if !settings.is_object() {
        *settings = json!({});
    }

    let hooks = settings.as_object_mut().and_then(|o| {
        o.entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
    });
    let Some(hooks) = hooks else { return };

    for event in HOOK_EVENTS {
        let matchers = hooks.entry(event).or_insert_with(|| json!([]));
        if !matchers.is_array() {
            *matchers = json!([]);
        }
        let Some(matchers) = matchers.as_array_mut() else {
            continue;
        };

        // Idempotent: installing twice must not leave two entries, or every
        // prompt would print the summary twice (SPEC §9.3).
        if matchers.iter().any(is_ours) {
            continue;
        }

        matchers.push(json!({
            "hooks": [{
                "type": "command",
                "command": HOOK_COMMAND,
            }]
        }));
    }
}

/// Remove hivemind's hooks, leaving everything else be.
pub(crate) fn unmerge_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };

    for event in HOOK_EVENTS {
        if let Some(matchers) = hooks.get_mut(event).and_then(Value::as_array_mut) {
            matchers.retain(|entry| !is_ours(entry));
        }
    }

    // Leave no empty scaffolding behind: an empty array we created is clutter
    // in someone else's configuration file.
    for event in HOOK_EVENTS {
        if hooks
            .get(event)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            hooks.remove(event);
        }
    }
}

/// Install the hooks (SPEC §9.3).
pub(crate) fn install() -> Result<()> {
    let path = claude_settings_path()?;
    let mut settings = read_settings(&path)?;
    merge_hooks(&mut settings);
    write_settings(&path, &settings)?;
    println!("hooks installed in {}", path.display());
    Ok(())
}

/// Remove the hooks (SPEC §9.3).
pub(crate) fn uninstall() -> Result<()> {
    let path = claude_settings_path()?;
    let mut settings = read_settings(&path)?;
    unmerge_hooks(&mut settings);
    write_settings(&path, &settings)?;
    println!("hooks removed from {}", path.display());
    Ok(())
}

/// The JSON snippet other MCP clients need (SPEC §9.2).
pub(crate) fn mcp_print(api: &str) -> Result<()> {
    let snippet = json!({
        "mcpServers": {
            "hivemind": {
                "type": "http",
                "url": format!("{}/mcp", api.trim_end_matches('/')),
            }
        }
    });
    println!("{}", serde_json::to_string_pretty(&snippet)?);
    Ok(())
}

/// Register the MCP server with Claude Code (SPEC §9.2).
pub(crate) fn mcp_install(api: &str) -> Result<()> {
    let url = format!("{}/mcp", api.trim_end_matches('/'));
    let args = [
        "mcp",
        "add",
        "--scope",
        "user",
        "--transport",
        "http",
        "hivemind",
        &url,
    ];

    match std::process::Command::new("claude").args(args).status() {
        Ok(status) if status.success() => {
            println!("registered with Claude Code");
            Ok(())
        }
        Ok(status) => {
            anyhow::bail!("`claude {}` exited with {status}", args.join(" "))
        }
        // Not having Claude Code on PATH is an ordinary situation, not a
        // failure: print the command so it can be run elsewhere (SPEC §2).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`claude` is not on your PATH. Run this where it is:");
            println!();
            println!("  claude {}", args.join(" "));
            Ok(())
        }
        Err(e) => Err(anyhow::Error::new(e).context("could not run `claude`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_into_an_empty_settings_file_adds_every_event() {
        let mut settings = json!({});
        merge_hooks(&mut settings);

        for event in HOOK_EVENTS {
            let matchers = settings["hooks"][event].as_array().expect("array");
            assert_eq!(matchers.len(), 1, "{event}");
            assert_eq!(matchers[0]["hooks"][0]["command"], HOOK_COMMAND);
        }
    }

    #[test]
    fn an_older_installation_gains_the_event_it_did_not_have() {
        // `SessionEnd` arrived with sessions (#52). Somebody who ran
        // `hook install` before that has the first two and not the third, and
        // running it again has to add the one that is missing without
        // doubling the two that are there.
        let mut settings = json!({});
        merge_hooks(&mut settings);
        settings["hooks"]
            .as_object_mut()
            .expect("object")
            .remove("SessionEnd");

        merge_hooks(&mut settings);

        for event in HOOK_EVENTS {
            let matchers = settings["hooks"][event].as_array().expect("array");
            assert_eq!(matchers.len(), 1, "{event}");
        }
    }

    #[test]
    fn installing_twice_does_not_duplicate_the_hook() {
        // Otherwise every prompt would print the unread summary twice.
        let mut settings = json!({});
        merge_hooks(&mut settings);
        merge_hooks(&mut settings);

        for event in HOOK_EVENTS {
            assert_eq!(settings["hooks"][event].as_array().expect("array").len(), 1);
        }
    }

    #[test]
    fn installing_preserves_hooks_that_are_not_ours() {
        // This is somebody's real configuration. Clobbering it would be
        // unforgivable (SPEC §2: "merging, never clobbering").
        let mut settings = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [{ "type": "command", "command": "echo hello" }] }
                ],
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "lint" }] }
                ]
            },
            "model": "opus",
            "theme": "dark"
        });
        merge_hooks(&mut settings);

        let session_start = settings["hooks"]["SessionStart"].as_array().expect("array");
        assert_eq!(session_start.len(), 2);
        assert_eq!(session_start[0]["hooks"][0]["command"], "echo hello");

        // Untouched: a different event, and unrelated top-level settings.
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["theme"], "dark");
    }

    #[test]
    fn uninstalling_removes_only_our_hook() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [{ "type": "command", "command": "echo hello" }] }
                ]
            }
        });
        merge_hooks(&mut settings);
        unmerge_hooks(&mut settings);

        let session_start = settings["hooks"]["SessionStart"].as_array().expect("array");
        assert_eq!(session_start.len(), 1);
        assert_eq!(session_start[0]["hooks"][0]["command"], "echo hello");
    }

    #[test]
    fn uninstalling_leaves_no_empty_scaffolding_behind() {
        let mut settings = json!({});
        merge_hooks(&mut settings);
        unmerge_hooks(&mut settings);

        let hooks = settings["hooks"].as_object().expect("object");
        assert!(hooks.is_empty(), "got {hooks:?}");
    }

    #[test]
    fn uninstalling_from_settings_we_never_touched_changes_nothing() {
        let original = json!({ "model": "opus" });
        let mut settings = original.clone();
        unmerge_hooks(&mut settings);
        assert_eq!(settings, original);
    }

    #[test]
    fn a_settings_file_whose_hooks_are_the_wrong_shape_is_repaired_not_lost() {
        // Whatever a `hooks.SessionStart` string means, it is not a matcher
        // list. Replace that key, keep the rest of the file.
        let mut settings = json!({ "hooks": { "SessionStart": "nonsense" }, "model": "opus" });
        merge_hooks(&mut settings);

        assert_eq!(
            settings["hooks"]["SessionStart"]
                .as_array()
                .expect("array")
                .len(),
            1
        );
        assert_eq!(settings["model"], "opus");
    }

    #[test]
    fn a_settings_document_that_is_not_an_object_is_replaced_rather_than_panicked_on() {
        let mut settings = json!([1, 2, 3]);
        merge_hooks(&mut settings);
        assert!(settings["hooks"]["SessionStart"].is_array());
    }

    #[test]
    fn settings_round_trip_through_a_real_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("settings.json");

        let mut settings = read_settings(&path).expect("missing file reads as empty");
        merge_hooks(&mut settings);
        write_settings(&path, &settings).expect("write");

        let back = read_settings(&path).expect("read");
        assert_eq!(
            back["hooks"]["SessionStart"]
                .as_array()
                .expect("array")
                .len(),
            1
        );
    }

    #[test]
    fn a_settings_file_that_is_not_json_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{ this is not json").expect("write");

        let error = read_settings(&path).expect_err("should refuse");
        assert!(
            error.to_string().contains("will not overwrite"),
            "got: {error}"
        );
    }

    #[test]
    fn writing_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write_settings(&path, &json!({ "a": 1 })).expect("write");

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["settings.json".to_owned()]);
    }
}
