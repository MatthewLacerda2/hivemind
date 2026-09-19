"""Every shape `judge` has to tell apart, without touching the network.

The cases are ones that have actually gone wrong: three found in `scorsese`,
and the first one in this repo's own merge loop.

`unittest` rather than `pytest` so the gate needs nothing installed. A test
gate that starts with "first, `pip install`" is one that gets skipped.
"""

import contextlib
import io
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from mergeable import judge, main  # noqa: E402

SHA = "1234567890abcdef"


def pull(**overrides) -> dict:
    """A ready pull request GitHub says merges cleanly."""
    return {
        "headRefOid": SHA,
        "isDraft": False,
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        **overrides,
    }


def run(run_id: int = 1, **overrides) -> dict:
    """A completed, successful CI run."""
    return {
        "id": run_id,
        "name": "CI",
        "head_sha": SHA,
        "status": "completed",
        "conclusion": "success",
        "html_url": f"https://example.invalid/{run_id}",
        **overrides,
    }


def jobs(run_id: int = 1, conclusion: str = "success") -> dict:
    return {run_id: [{"name": "check", "conclusion": conclusion}]}


class Mergeable(unittest.TestCase):
    def test_a_run_that_passed_having_built_something_is_mergeable(self):
        ok, why = judge(pull(), [run()], jobs())
        self.assertTrue(ok, why)

    def test_a_skipped_run_beside_a_real_one_decides_nothing(self):
        runs = [run(1, conclusion="skipped"), run(2)]
        all_jobs = {1: [{"conclusion": "skipped"}], 2: [{"conclusion": "success"}]}
        ok, why = judge(pull(), runs, all_jobs)
        self.assertTrue(ok, why)


class NotMergeable(unittest.TestCase):
    def test_no_runs_at_all(self):
        # The failure this script exists for. The loop it replaces read zero
        # checks as settled, printed an empty list of conclusions and merged.
        ok, why = judge(pull(), [], {})
        self.assertFalse(ok)
        self.assertIn("no CI run exists", why[0])

    def test_no_runs_on_a_conflicted_branch_names_the_conflict(self):
        # Same symptom as a lost run, different file to go and edit. Getting
        # this wrong sends a reader at the workflow YAML.
        ok, why = judge(
            pull(mergeable="CONFLICTING", mergeStateStatus="DIRTY"), [], {}
        )
        self.assertFalse(ok)
        self.assertTrue(any("conflicts with `main`" in line for line in why))
        self.assertTrue(any("do not go and edit it" in line for line in why))

    def test_either_conflict_field_alone_is_enough(self):
        # They have been seen to lag each other by a poll.
        for field in ("mergeable", "mergeStateStatus"):
            with self.subTest(field=field):
                ok, why = judge(pull(**{field: "CONFLICTING"}), [], {})
                self.assertFalse(ok)
                self.assertTrue(any("conflicts with `main`" in l for l in why))

    def test_mergeability_not_yet_computed_is_said_out_loud(self):
        # Silence would read as "checked, and it is not a conflict".
        ok, why = judge(pull(mergeable="UNKNOWN", mergeStateStatus="UNKNOWN"), [], {})
        self.assertFalse(ok)
        self.assertTrue(any("not finished computing" in line for line in why))

    def test_a_run_whose_every_job_was_skipped(self):
        # It concludes `success` having compiled nothing: an absent check
        # wearing the costume of a passing one.
        ok, why = judge(pull(), [run()], jobs(conclusion="skipped"))
        self.assertFalse(ok)
        self.assertIn("every CI job", why[0])

    def test_a_failing_run_beside_a_green_one(self):
        # The direction that matters. Picking a run by list position could call
        # this commit mergeable, and nothing about "first" prefers green.
        runs = [run(1), run(2, conclusion="failure")]
        all_jobs = {1: [{"conclusion": "success"}], 2: [{"conclusion": "failure"}]}
        ok, why = judge(pull(), runs, all_jobs)
        self.assertFalse(ok)
        self.assertIn("failure", why[0])

    def test_the_verdict_does_not_depend_on_the_order_the_api_returned(self):
        green, red = run(1), run(2, conclusion="failure")
        all_jobs = {1: [{"conclusion": "success"}], 2: [{"conclusion": "failure"}]}
        self.assertFalse(judge(pull(), [green, red], all_jobs)[0])
        self.assertFalse(judge(pull(), [red, green], all_jobs)[0])

    def test_a_run_still_going(self):
        ok, why = judge(pull(), [run(status="in_progress", conclusion=None)], jobs())
        self.assertFalse(ok)
        self.assertIn("still in_progress", why[0])

    def test_a_cancelled_run_is_a_failure(self):
        ok, _ = judge(pull(), [run(conclusion="cancelled")], jobs())
        self.assertFalse(ok)

    def test_a_skipped_run_alone_has_still_built_nothing(self):
        # A skipped *run* is not a failure — it is what CI does to a change it
        # ignores by design — so the every-job-skipped rule is what refuses it.
        ok, why = judge(
            pull(), [run(conclusion="skipped")], jobs(conclusion="skipped")
        )
        self.assertFalse(ok)
        self.assertIn("every CI job", why[0])

    def test_a_draft(self):
        ok, why = judge(pull(isDraft=True), [run()], jobs())
        self.assertFalse(ok)
        self.assertIn("draft", why[0])


class Arguments(unittest.TestCase):
    """`just mergeable PR=12` was documented here from the first commit and
    never worked: `PR` is a recipe parameter, so the whole string arrives as
    its value. The number goes positionally (#48).
    """

    def refuse(self, arg: str) -> tuple[int, str]:
        said = io.StringIO()
        with contextlib.redirect_stderr(said):
            code = main(["mergeable.py", arg])
        return code, said.getvalue()

    def test_a_pr_prefixed_argument_names_the_form_that_works(self):
        code, said = self.refuse("PR=12")
        self.assertEqual(code, 2)
        self.assertIn("just mergeable 12", said)

    def test_it_echoes_back_the_number_it_was_actually_given(self):
        _, said = self.refuse("PR=4071")
        self.assertIn("just mergeable 4071", said)

    def test_an_ordinary_bad_argument_gets_the_ordinary_usage(self):
        code, said = self.refuse("banana")
        self.assertEqual(code, 2)
        self.assertNotIn("just mergeable", said)


if __name__ == "__main__":
    unittest.main()
