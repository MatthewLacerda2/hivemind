//! One place that decides whether output is coloured (SPEC §10).
//!
//! SPEC §10 promises "colors via `owo-colors`, respecting `NO_COLOR`", and
//! nothing did. `.green()` and `.bold()` emit their escape codes
//! unconditionally, so `NO_COLOR=1` changed nothing and neither did piping the
//! output somewhere that is not a terminal.
//!
//! It bites exactly when the reader is a machine. `hivemind group create`
//! printed its code `.bold()`, and an integration test reading that code off
//! stdout got `\x1b[1mhm-…\x1b[0m`, which does not parse (#50). The fix then
//! was to stop styling that one line. This is the general answer — and the
//! rest of the CLI has the same problem the moment a script, a pipe, or a
//! Claude reads what `hivemind peers` printed.
//!
//! # Why a trait rather than `set_override`
//!
//! `owo_colors::set_override` only reaches `if_supports_color`. The 65 call
//! sites here use `.green()` and friends directly, and rewriting every one of
//! them into a closure would be a worse thing to read for the same result.
//!
//! So [`Paint`] offers the same method names, consults one flag, and delegates
//! the actual escape codes to `owo-colors`. Call sites are untouched; the four
//! files that styled anything changed one `use` line each.

use std::fmt::Display;
use std::io::IsTerminal as _;
use std::sync::atomic::{AtomicBool, Ordering};

use owo_colors::OwoColorize;

/// Whether to emit escape codes. Decided once, at startup.
static COLOUR: AtomicBool = AtomicBool::new(false);

/// Should output be coloured?
///
/// A pure judgement so both answers can be tested on a machine that is
/// whichever one it happens to be — the shape `doctor`'s optional-tools rule
/// and presence's `still_here` took, for the same reason.
///
/// `NO_COLOR` follows the convention it comes from: **present and non-empty**
/// disables colour. `NO_COLOR=` is how a shell unsets a variable, and reading
/// that as "no colour" would make the variable impossible to turn off.
#[must_use]
pub(crate) fn wanted(no_color: Option<&str>, is_terminal: bool) -> bool {
    if no_color.is_some_and(|value| !value.is_empty()) {
        return false;
    }
    is_terminal
}

/// Decide once, for this process.
///
/// Called from `main` before anything prints. Anything that runs earlier than
/// this gets no colour, which is the safe direction to be wrong in.
pub(crate) fn init() {
    let no_color = std::env::var("NO_COLOR").ok();
    COLOUR.store(
        wanted(no_color.as_deref(), std::io::stdout().is_terminal()),
        Ordering::Relaxed,
    );
}

fn on() -> bool {
    COLOUR.load(Ordering::Relaxed)
}

/// The colour methods, gated on one decision.
///
/// The same names `owo-colors` uses, so that using this in place of
/// `OwoColorize` is a change of `use` line and nothing else. Each returns a
/// `String` rather than a styled wrapper: the wrapper's whole purpose is to
/// defer the escape codes, and by here they have already been decided.
pub(crate) trait Paint: Display {
    fn bold(&self) -> String;
    fn dimmed(&self) -> String;
    fn green(&self) -> String;
    fn yellow(&self) -> String;
    fn red(&self) -> String;
    fn cyan(&self) -> String;
    fn magenta(&self) -> String;
}

/// Implemented for everything printable, which is what keeps the call sites
/// identical — including `"…".green().bold()`, where the first call produces a
/// `String` that the second one can style in turn.
impl<T: Display> Paint for T {
    fn bold(&self) -> String {
        if on() {
            OwoColorize::bold(self).to_string()
        } else {
            self.to_string()
        }
    }

    fn dimmed(&self) -> String {
        if on() {
            OwoColorize::dimmed(self).to_string()
        } else {
            self.to_string()
        }
    }

    fn green(&self) -> String {
        if on() {
            OwoColorize::green(self).to_string()
        } else {
            self.to_string()
        }
    }

    fn yellow(&self) -> String {
        if on() {
            OwoColorize::yellow(self).to_string()
        } else {
            self.to_string()
        }
    }

    fn red(&self) -> String {
        if on() {
            OwoColorize::red(self).to_string()
        } else {
            self.to_string()
        }
    }

    fn cyan(&self) -> String {
        if on() {
            OwoColorize::cyan(self).to_string()
        } else {
            self.to_string()
        }
    }

    fn magenta(&self) -> String {
        if on() {
            OwoColorize::magenta(self).to_string()
        } else {
            self.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_with_nothing_said_about_it_gets_colour() {
        assert!(wanted(None, true));
    }

    #[test]
    fn no_color_turns_it_off_even_on_a_terminal() {
        // The promise SPEC §10 makes and nothing kept.
        assert!(!wanted(Some("1"), true));
        assert!(!wanted(Some("anything at all"), true));
    }

    #[test]
    fn an_empty_no_color_is_not_set() {
        // `NO_COLOR=` is how a shell unsets a variable, and the convention
        // this follows says present *and non-empty*. Reading the empty string
        // as "no colour" would make the variable impossible to turn back off.
        assert!(wanted(Some(""), true));
    }

    #[test]
    fn output_that_is_not_a_terminal_gets_no_colour() {
        // The half that bit first: a test reading `hivemind group create`'s
        // output got `\x1b[1mhm-…\x1b[0m`, which does not parse (#50).
        assert!(!wanted(None, false));
        assert!(!wanted(Some("1"), false));
    }

    #[test]
    fn the_flag_is_what_decides_and_both_answers_are_checked() {
        // One test rather than two: `COLOUR` is process-wide and the test
        // harness is threaded, so two tests setting it would race each other
        // into whichever answer ran last.
        // Qualified, because `OwoColorize` is in scope here too — it is what
        // the implementation delegates to — and an unqualified `.green()`
        // would be ambiguous. Call sites elsewhere import only `Paint`.
        let chain = |text: &str| Paint::bold(&Paint::green(&Paint::dimmed(&text)));

        COLOUR.store(false, Ordering::Relaxed);
        assert_eq!(Paint::green(&"plain"), "plain");
        assert_eq!(Paint::bold(&"plain"), "plain");
        assert_eq!(
            chain("plain"),
            "plain",
            "chained styles collapse too, or `.green().bold()` would leak one"
        );

        // The other side, so the assertions above cannot pass by the trait
        // doing nothing at all.
        COLOUR.store(true, Ordering::Relaxed);
        assert!(Paint::green(&"bright").contains('\u{1b}'));
        assert!(chain("bright").contains('\u{1b}'));

        COLOUR.store(false, Ordering::Relaxed);
    }
}
