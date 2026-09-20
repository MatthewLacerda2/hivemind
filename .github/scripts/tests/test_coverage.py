"""The coverage verdict, and every way of not having a number.

The interesting cases are all the second kind. A percentage below the floor
fails, which is easy; output that carries no percentage at all must fail too,
because "nothing was measured" reads as "0%" to a scraper and as "fine" to
anybody who only looks for the word FAILED.
"""

import contextlib
import io
import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

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


if __name__ == "__main__":
    unittest.main()
