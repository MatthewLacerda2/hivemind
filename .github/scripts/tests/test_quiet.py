"""What a quiet wrapper must never get wrong.

Two of these are the whole point of the file under test, and both are the same
mistake wearing different clothes: **a tick that was not earned**. One is a
command that printed nothing because it never ran; the other is a sweep over an
empty list of gates. `CLAUDE.md` records the second one happening for real, in
the wait-for-CI loop that counted unfinished checks and found zero of them.

`unittest` rather than `pytest`, so the gate needs nothing installed.
"""

import contextlib
import io
import os
import pathlib
import subprocess
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import quiet  # noqa: E402
from quiet import (  # noqa: E402
    SKIPPED,
    Outcome,
    main,
    parse,
    run,
    run_gates,
    verdict,
)


def lines_of(outcome: Outcome) -> str:
    return "\n".join(outcome.lines)


class Verdict(unittest.TestCase):
    """The judgement, which reads the exit status and nothing else."""

    def test_a_clean_run_is_one_line_naming_the_gate(self):
        out = verdict("lint", 0, 12.3, "warning: something\n" * 200)
        self.assertEqual(out.state, quiet.OK)
        self.assertEqual(out.status, 0)
        self.assertEqual(len(out.lines), 1)
        self.assertIn("lint", out.lines[0])

    def test_a_clean_run_keeps_none_of_the_output_it_swallowed(self):
        out = verdict("test", 0, 1.0, "test result: ok. 450 passed\n")
        self.assertNotIn("450", lines_of(out))

    def test_tail_keeps_the_line_that_counts_what_ran(self):
        # "the suite passed" and "the suite was empty" are different states, and
        # nextest's last line is the only place the difference shows.
        out = verdict("test", 0, 1.0, "a\nb\nSummary: 451 tests run\n", tail=1)
        self.assertIn("451 tests run", lines_of(out))
        self.assertNotIn("\na\n", lines_of(out))

    def test_tail_over_silence_says_so_rather_than_inventing_a_summary(self):
        out = verdict("test", 0, 1.0, "", tail=1)
        self.assertIn("no output", lines_of(out))

    def test_a_failure_carries_the_whole_output(self):
        out = verdict("lint", 101, 3.0, "error: one\nerror: two\nerror: three\n")
        self.assertEqual(out.state, quiet.FAIL)
        for expected in ("one", "two", "three"):
            self.assertIn(expected, lines_of(out))

    def test_a_failure_keeps_the_exit_status_it_was_given(self):
        # A wrapper that normalised this would be the false green.
        self.assertEqual(verdict("x", 101, 0.0, "").status, 101)
        self.assertEqual(verdict("x", 1, 0.0, "").status, 1)
        self.assertEqual(verdict("x", 2, 0.0, "").status, 2)

    def test_a_silent_failure_is_still_a_failure_and_says_it_was_silent(self):
        # The case this file exists for: no output to grep, so a summariser that
        # looked for an error string would have found none and printed a tick.
        out = verdict("doc", 1, 0.0, "")
        self.assertEqual(out.state, quiet.FAIL)
        self.assertNotEqual(out.status, 0)
        self.assertIn("no output", lines_of(out))

    def test_a_command_killed_by_a_signal_has_not_passed(self):
        out = verdict("test", -9, 60.0, "")
        self.assertEqual(out.state, quiet.FAIL)
        self.assertEqual(out.status, 137)
        self.assertIn("SIGKILL", lines_of(out))

    def test_a_skip_is_neither_a_pass_nor_a_failure(self):
        out = verdict("dist-check", SKIPPED, 0.1, "dist: not installed")
        self.assertEqual(out.state, quiet.SKIP)
        # The caller is entitled to carry on — the gate declined, by design.
        self.assertEqual(out.status, 0)
        # But it must not read as a check that passed.
        self.assertNotIn("ok ", lines_of(out))
        self.assertIn("not installed", lines_of(out))

    def test_a_skip_without_a_reason_admits_it_has_none(self):
        out = verdict("dist-check", SKIPPED, 0.1, "")
        self.assertIn("no reason given", lines_of(out))


class Running(unittest.TestCase):
    """Spawning, which is the only thing allowed to produce a tick."""

    def test_a_real_command_that_passes(self):
        out = run("echo", [sys.executable, "-c", "print('hello')"])
        self.assertEqual(out.state, quiet.OK)
        self.assertNotIn("hello", lines_of(out))

    def test_a_real_command_that_fails_shows_both_streams(self):
        out = run(
            "boom",
            [
                sys.executable,
                "-c",
                "import sys; print('out'); print('err', file=sys.stderr);"
                " sys.exit(3)",
            ],
        )
        self.assertEqual(out.status, 3)
        self.assertIn("out", lines_of(out))
        self.assertIn("err", lines_of(out))

    def test_a_command_that_cannot_be_spawned_has_not_passed(self):
        out = run("ghost", ["hivemind-no-such-command-71"])
        self.assertEqual(out.state, quiet.FAIL)
        self.assertEqual(out.status, 127)

    def test_children_are_told_they_are_already_wrapped(self):
        out = run(
            "nested",
            [
                sys.executable,
                "-c",
                f"import os, sys; sys.exit(0 if os.environ.get('{quiet.WRAPPED}')"
                " else 1)",
            ],
        )
        self.assertEqual(out.state, quiet.OK)


class Durations(unittest.TestCase):
    """A total of `0.0m` is a number nobody can use."""

    def test_short_runs_are_in_seconds(self):
        self.assertEqual(quiet.elapsed(0.4), "0s")
        self.assertEqual(quiet.elapsed(12.6), "13s")

    def test_long_runs_are_in_minutes(self):
        self.assertEqual(quiet.elapsed(300.0), "5.0m")


class Gates(unittest.TestCase):
    """The sweep, and the empty sweep that must not look like a clean one."""

    def fake(self, outcomes):
        """A runner that answers from a script, recording what it was asked."""
        self.asked = []

        def runner(label, command, verbose=False, tail=0):
            self.asked.append(label)
            return outcomes[label]

        return runner

    def test_no_gates_is_not_a_clean_sweep(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = run_gates([])
        self.assertNotEqual(code, 0)
        self.assertIn("not the same as passing", err.getvalue())

    def test_every_gate_gets_one_line_and_the_sweep_passes(self):
        outcomes = {
            name: Outcome(name, quiet.OK, 0, 1.0, [f"ok {name}"])
            for name in ("fmt-check", "lint", "test")
        }
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = run_gates(list(outcomes), runner=self.fake(outcomes))
        self.assertEqual(code, 0)
        self.assertEqual(self.asked, ["fmt-check", "lint", "test"])
        self.assertIn("3 of 3 gates ok", out.getvalue())

    def test_the_sweep_stops_at_the_first_failure_and_names_what_did_not_run(self):
        outcomes = {
            "fmt-check": Outcome("fmt-check", quiet.OK, 0, 1.0, ["ok"]),
            "lint": Outcome("lint", quiet.FAIL, 101, 1.0, ["FAILED", "error: nope"]),
            "test": Outcome("test", quiet.OK, 0, 1.0, ["ok"]),
        }
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = run_gates(["fmt-check", "lint", "test"], runner=self.fake(outcomes))
        self.assertEqual(code, 101)
        self.assertEqual(self.asked, ["fmt-check", "lint"])
        printed = out.getvalue()
        self.assertIn("error: nope", printed)
        self.assertIn("FAILED at lint", printed)
        self.assertIn("1 not run", printed)

    def test_a_skipped_gate_is_counted_apart_from_the_ones_that_passed(self):
        outcomes = {
            "lint": Outcome("lint", quiet.OK, 0, 1.0, ["ok"]),
            "dist-check": Outcome("dist-check", quiet.SKIP, SKIPPED, 0.1, ["skip"]),
        }
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = run_gates(["lint", "dist-check"], runner=self.fake(outcomes))
        self.assertEqual(code, 0)
        self.assertIn("1 of 2 gates ok", out.getvalue())
        self.assertIn("1 skipped", out.getvalue())

    def test_a_gate_is_a_just_recipe(self):
        self.assertEqual(quiet.gate_command("cov-gate"), ["just", "cov-gate"])


class Parsing(unittest.TestCase):
    """The command line, which has to survive a command containing `--`."""

    def test_only_the_first_separator_is_ours(self):
        call = parse(
            ["--label", "lint", "--", "cargo", "clippy", "--", "-D", "warnings"]
        )
        self.assertEqual(call.label, "lint")
        self.assertEqual(call.command, ["cargo", "clippy", "--", "-D", "warnings"])

    def test_verbose_from_the_command_line_or_the_environment(self):
        self.assertTrue(parse(["--verbose", "--label", "x", "--", "true"]).verbose)
        self.assertTrue(parse(["--label", "x", "--", "true"], {quiet.VERBOSE: "1"}).verbose)
        self.assertFalse(parse(["--label", "x", "--", "true"], {}).verbose)

    def test_gates_collect_until_the_next_option(self):
        call = parse(["--gates", "lint", "test", "--verbose"])
        self.assertEqual(call.gates, ["lint", "test"])
        self.assertTrue(call.verbose)

    def test_tail_is_a_number(self):
        self.assertEqual(parse(["--tail", "2", "--", "true"]).tail, 2)

    def test_an_unknown_argument_is_refused_rather_than_ignored(self):
        with self.assertRaises(ValueError):
            parse(["--quiet-ish", "--", "true"])


class Main(unittest.TestCase):
    """The entry point, end to end, without `just`."""

    def test_a_passing_command_prints_one_line_and_exits_zero(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = main(
                ["quiet.py", "--label", "echo", "--", "echo", "noise"], env={}
            )
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue().strip().count("\n"), 0)
        self.assertNotIn("noise", out.getvalue())

    def test_a_failing_command_keeps_its_status_and_shows_its_output(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = main(
                [
                    "quiet.py",
                    "--label",
                    "boom",
                    "--",
                    sys.executable,
                    "-c",
                    "import sys; print('the reason'); sys.exit(7)",
                ],
                env={},
            )
        self.assertEqual(code, 7)
        self.assertIn("the reason", out.getvalue())

    def test_nothing_to_run_is_refused(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(main(["quiet.py", "--label", "x"], env={}), 2)

    def test_a_bad_argument_prints_the_usage(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(main(["quiet.py", "--nope"], env={}), 2)
        self.assertIn("usage:", err.getvalue())


class Nesting(unittest.TestCase):
    """A wrapper inside a wrapper says nothing and changes nothing."""

    def setUp(self):
        self.script = pathlib.Path(quiet.__file__)

    def test_the_decision_is_read_from_the_environment_it_is_given(self):
        # The bug this argument exists for: with WRAPPED inherited from a sweep,
        # the wrapper passed the command through and this suite's own tests
        # judged a different code path than the one they name.
        code = main(
            [
                "quiet.py",
                "--label",
                "inner",
                "--",
                sys.executable,
                "-c",
                "import sys; sys.exit(5)",
            ],
            env={quiet.WRAPPED: "outer"},
        )
        self.assertEqual(code, 5)

    def test_an_inner_wrapper_passes_the_output_and_the_status_through(self):
        done = subprocess.run(
            [
                sys.executable,
                str(self.script),
                "--label",
                "inner",
                "--",
                sys.executable,
                "-c",
                "import sys; print('inner noise'); sys.exit(5)",
            ],
            env={**os.environ, quiet.WRAPPED: "outer"},
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        self.assertEqual(done.returncode, 5)
        self.assertIn("inner noise", done.stdout)
        # No summary line of its own: the outer wrapper is reporting.
        self.assertNotIn("FAILED", done.stdout)


if __name__ == "__main__":
    unittest.main()
