"""The coverage verdict, and every way of not having a number.

The interesting cases are all the second kind. A percentage below the floor
fails, which is easy; output that carries no percentage at all must fail too,
because "nothing was measured" reads as "0%" to a scraper and as "fine" to
anybody who only looks for the word FAILED.

The last class here is about the other half of the same incident: the
expression that decides which files the gate measures at all.
"""

import contextlib
import io
import json
import pathlib
import re
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

JUSTFILE = pathlib.Path(__file__).resolve().parents[3] / "justfile"

from coverage import Lines, line_totals, main, parse, verdict  # noqa: E402


def export(count=1000, covered=900, percent=90.0) -> str:
    """What `cargo llvm-cov report --json --summary-only` prints."""
    return json.dumps(
        {
            "data": [
                {
                    "totals": {
                        "lines": {
                            "count": count,
                            "covered": covered,
                            "percent": percent,
                        },
                        "functions": {"count": 10, "covered": 9, "percent": 90.0},
                    }
                }
            ],
            "type": "llvm.coverage.json.export",
            "version": "2.0.1",
        }
    )


class Totals(unittest.TestCase):
    def test_the_line_totals_come_out_of_the_export(self):
        self.assertEqual(line_totals(export()), Lines(1000, 900, 90.0))

    def test_empty_output_has_no_totals(self):
        self.assertIsNone(line_totals(""))

    def test_a_diagnostic_instead_of_json_has_no_totals(self):
        self.assertIsNone(line_totals("error: no profraw files found\n"))

    def test_a_warning_on_the_same_stream_does_not_lose_the_number(self):
        noisy = "warning: some crates have no coverage\n" + export() + "\n"
        self.assertEqual(line_totals(noisy), Lines(1000, 900, 90.0))

    def test_json_of_another_shape_has_no_totals(self):
        self.assertIsNone(line_totals("[]"))
        self.assertIsNone(line_totals("{}"))
        self.assertIsNone(line_totals('{"data": []}'))
        self.assertIsNone(line_totals('{"data": [{"totals": {}}]}'))

    def test_a_report_over_no_lines_at_all_has_no_totals(self):
        # Zero lines instrumented is 0%, and 0% of nothing is not a measurement.
        self.assertIsNone(line_totals(export(count=0, covered=0, percent=0.0)))


class Verdict(unittest.TestCase):
    def test_above_the_floor_is_one_line_with_the_number_and_the_floor(self):
        ok, line = verdict(Lines(1000, 900, 90.0), 85)
        self.assertTrue(ok)
        self.assertIn("90.00%", line)
        self.assertIn("85%", line)

    def test_below_the_floor_fails_and_still_shows_the_number(self):
        ok, line = verdict(Lines(1000, 800, 80.0), 85)
        self.assertFalse(ok)
        self.assertIn("FAILED", line)
        self.assertIn("80.00%", line)

    def test_exactly_the_floor_holds(self):
        ok, _ = verdict(Lines(1000, 850, 85.0), 85)
        self.assertTrue(ok)

    def test_no_measurement_is_a_failure_that_says_why(self):
        ok, line = verdict(None, 85)
        self.assertFalse(ok)
        self.assertIn("FAILED", line)
        self.assertIn("not the same as passing", line)

    def test_the_verdict_never_prints_the_per_file_table(self):
        # The whole point of #71: the table belongs to `just cov`.
        _, line = verdict(Lines(1000, 900, 90.0), 85)
        self.assertEqual(line.count("\n"), 0)


class Parsing(unittest.TestCase):
    def test_the_floor_and_the_command(self):
        floor, command = parse(["--floor", "85", "--", "cargo", "llvm-cov", "--json"])
        self.assertEqual(floor, 85.0)
        self.assertEqual(command, ["cargo", "llvm-cov", "--json"])

    def test_an_unknown_argument_is_refused(self):
        with self.assertRaises(ValueError):
            parse(["--nope"])


class Main(unittest.TestCase):
    def test_a_report_above_the_floor_exits_zero(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = main(
                [
                    "coverage.py",
                    "--floor",
                    "85",
                    "--",
                    sys.executable,
                    "-c",
                    f"print({export()!r})",
                ]
            )
        self.assertEqual(code, 0)
        self.assertIn("90.00%", out.getvalue())

    def test_a_report_command_that_fails_is_not_a_pass(self):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = main(
                [
                    "coverage.py",
                    "--floor",
                    "85",
                    "--",
                    sys.executable,
                    "-c",
                    "import sys; print('error: no profraw files',"
                    " file=sys.stderr); sys.exit(1)",
                ]
            )
        self.assertEqual(code, 1)
        self.assertIn("FAILED", out.getvalue())

    def test_a_report_command_that_prints_json_but_fails_is_not_a_pass(self):
        # Exit status and a parseable number are both required; either alone is
        # evidence of nothing.
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = main(
                [
                    "coverage.py",
                    "--floor",
                    "85",
                    "--",
                    sys.executable,
                    "-c",
                    f"import sys; print({export()!r}); sys.exit(2)",
                ]
            )
        self.assertEqual(code, 1)
        self.assertIn("FAILED", out.getvalue())

    def test_a_report_command_that_is_not_there_is_not_a_pass(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = main(
                ["coverage.py", "--floor", "85", "--", "hivemind-no-such-command-71"]
            )
        self.assertEqual(code, 1)
        self.assertIn("FAILED", out.getvalue())

    def test_no_command_is_refused(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(main(["coverage.py", "--floor", "85"]), 2)


# A worktree per branch is how this repository is worked, and a branch that
# touches tests is habitually named for it. This is the one that broke the
# gate (#91); the paths below are the shape llvm-cov actually sees.
TESTS_WORKTREE = "/Users/dev/repos/hivemind.worktrees/split-local-tests"
PLAIN_WORKTREE = "/Users/dev/repos/hivemind"


def ignore_expression() -> str:
    """The expression the justfile hands `cargo llvm-cov --ignore-filename-regex`.

    Read from the justfile rather than restated here, because a copy of it in
    this file would be a second place to be wrong, and a test asserting on the
    copy would pass while the gate stayed broken.
    """
    for line in JUSTFILE.read_text().splitlines():
        found = re.fullmatch(r"COV_IGNORE\s*:=\s*'(.*)'", line.strip())
        if found:
            return found.group(1)
    raise AssertionError(f"no COV_IGNORE in {JUSTFILE}")


class IgnoredFiles(unittest.TestCase):
    """llvm-cov searches this expression against each file's *absolute* path.

    `re.search` is the same question llvm-cov asks — Rust's `regex` and
    Python's `re` agree on this subset, which is character classes and an
    alternation.
    """

    def ignored(self, path: str) -> bool:
        return re.search(ignore_expression(), path) is not None

    def test_a_worktree_named_after_tests_does_not_hide_the_workspace(self):
        # The failure this came from: every file ignored, an empty report, and
        # a floor of 85% passing over nothing at all.
        for path in (
            f"{TESTS_WORKTREE}/crates/hivemind-api/src/local.rs",
            f"{TESTS_WORKTREE}/crates/hivemind-core/src/index.rs",
            f"{TESTS_WORKTREE}/crates/hivemind-net/src/outbox.rs",
        ):
            with self.subTest(path=path):
                self.assertFalse(self.ignored(path))

    def test_the_source_under_measurement_is_measured(self):
        for path in (
            f"{PLAIN_WORKTREE}/crates/hivemind-core/src/store.rs",
            f"{PLAIN_WORKTREE}/crates/hivemind-net/src/transport.rs",
            f"{PLAIN_WORKTREE}/crates/hivemind-api/src/service.rs",
        ):
            with self.subTest(path=path):
                self.assertFalse(self.ignored(path))

    def test_the_integration_tests_and_the_cli_are_ignored_either_way(self):
        for root in (PLAIN_WORKTREE, TESTS_WORKTREE):
            for path in (
                f"{root}/crates/hivemind-net/tests/delivery.rs",
                f"{root}/crates/hivemind-cli/tests/end_to_end.rs",
                f"{root}/crates/hivemind-cli/src/commands.rs",
            ):
                with self.subTest(path=path):
                    self.assertTrue(self.ignored(path))


if __name__ == "__main__":
    unittest.main()
