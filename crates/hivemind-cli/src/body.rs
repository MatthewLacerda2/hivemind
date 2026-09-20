//! Where the body of a message comes from (SPEC §10).
//!
//! `send` and `reply` take the text of a message three ways: after `--`, as
//! `--body`, or on stdin. The `--` came first and it is there for a good
//! reason — a body that starts with a hyphen has to survive the argument
//! parser — but it is paid for on every call for a case that is rare, and it
//! is the first thing somebody got wrong using the program (#38). Subject and
//! body are the same kind of thing and arrived by two different conventions.
//!
//! The judgement below is a pure function taking "is stdin a terminal" as an
//! argument, and the lookup is one line in [`resolve`]. That split is the
//! point: a test for the piped case would otherwise pass or fail according to
//! how the test binary happened to be invoked, which is how `doctor`'s
//! optional-tools branch and `refresh_peers`'s tailnet branch both went
//! untested (CLAUDE.md).

use anyhow::{Context, Result, bail};

/// Where the body is to be read from, once the arguments have been judged.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Text that came in on the command line.
    Text(String),
    /// Everything on stdin, to end of file.
    Stdin,
}

/// Judge where the body should come from.
///
/// `option` is `--body`, `positional` is what followed `--` (or, for `reply`,
/// the trailing argument). Either spelling accepts `-`, which means stdin
/// whether or not stdin is a terminal — that is the one way to type a body in
/// by hand.
///
/// Two rules decide the awkward cases:
///
/// - **Two explicit bodies is a refusal, not a guess.** A caller that wrote
///   both `--body` and `-- …` meant two different things by them and only one
///   can be sent; picking silently would send the other one (#28, #69).
/// - **An explicit body beats a pipe.** Stdin being redirected is ambient —
///   a script, a `< /dev/null`, a runner that hands every child a pipe — so it
///   is the fallback rather than a claim, and it never overrides an argument
///   somebody typed.
pub(crate) fn choose(
    option: Option<&str>,
    positional: Option<&str>,
    stdin_is_terminal: bool,
) -> Result<Source> {
    let given = match (option, positional) {
        (Some(_), Some(_)) => {
            bail!("the body was given twice, as `--body` and after `--` — pass it once, either way")
        }
        (Some(text), None) | (None, Some(text)) => Some(text),
        (None, None) => None,
    };

    match given {
        Some("-") => Ok(Source::Stdin),
        Some(text) => Ok(Source::Text(text.to_owned())),
        None if stdin_is_terminal => bail!(
            "no message body — pass it with `--body`, after `--`, or pipe it in; `-` reads stdin even from a terminal"
        ),
        None => Ok(Source::Stdin),
    }
}

/// Produce the body itself, reading stdin if that is where it lives.
pub(crate) fn read(source: Source) -> Result<String> {
    match source {
        Source::Text(text) => Ok(text),
        Source::Stdin => {
            use std::io::Read as _;
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("could not read the message body from stdin")?;
            if buffer.trim().is_empty() {
                bail!("the message body is empty");
            }
            Ok(buffer)
        }
    }
}

/// Judge, then read: what `send` and `reply` call.
pub(crate) fn resolve(option: Option<&str>, positional: Option<&str>) -> Result<String> {
    use std::io::IsTerminal as _;
    read(choose(option, positional, std::io::stdin().is_terminal())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_option_spelling_carries_the_body() {
        assert_eq!(
            choose(Some("hello"), None, true).expect("a body"),
            Source::Text("hello".to_owned())
        );
    }

    #[test]
    fn the_trailing_spelling_carries_the_body() {
        assert_eq!(
            choose(None, Some("hello"), true).expect("a body"),
            Source::Text("hello".to_owned())
        );
    }

    #[test]
    fn both_spellings_at_once_is_refused() {
        let complaint = choose(Some("one"), Some("two"), false)
            .expect_err("two bodies cannot both be sent")
            .to_string();
        assert!(complaint.contains("twice"), "unhelpful: {complaint}");
    }

    #[test]
    fn a_typed_body_wins_over_a_pipe() {
        assert_eq!(
            choose(Some("typed"), None, false).expect("a body"),
            Source::Text("typed".to_owned())
        );
        assert_eq!(
            choose(None, Some("typed"), false).expect("a body"),
            Source::Text("typed".to_owned())
        );
    }

    #[test]
    fn no_body_and_a_pipe_reads_stdin() {
        assert_eq!(choose(None, None, false).expect("stdin"), Source::Stdin);
    }

    #[test]
    fn no_body_and_a_terminal_says_so_rather_than_waiting() {
        let complaint = choose(None, None, true)
            .expect_err("a terminal is not read from by accident")
            .to_string();
        assert!(complaint.contains("--body"), "unhelpful: {complaint}");
    }

    #[test]
    fn a_dash_reads_stdin_from_a_terminal_too() {
        assert_eq!(choose(Some("-"), None, true).expect("stdin"), Source::Stdin);
        assert_eq!(choose(None, Some("-"), true).expect("stdin"), Source::Stdin);
    }

    #[test]
    fn text_is_handed_back_without_touching_stdin() {
        assert_eq!(
            read(Source::Text("body".to_owned())).expect("a body"),
            "body"
        );
    }
}
