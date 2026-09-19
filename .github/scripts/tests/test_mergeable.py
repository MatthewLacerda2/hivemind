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

import mergeable  # noqa: E402
from mergeable import judge, main, unsettled  # noqa: E402

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


class Unsettled(unittest.TestCase):
    """Whether asking again could give a different answer.

    The question `judge` deliberately does not answer. Collapsing the two is
    what makes a hand-rolled `until` loop spin forever on a red pull request
    — and what made the one this script replaced read zero checks as green.
    """

    def test_no_runs_yet_is_worth_waiting_for(self):
        # A run can appear moments after a push, and that gap is most of the
        # reason anybody waits at all.
        self.assertTrue(unsettled(pull(), []))

    def test_a_run_still_going_is_worth_waiting_for(self):
        self.assertTrue(unsettled(pull(), [run(status="in_progress", conclusion=None)]))

    def test_a_red_run_is_settled(self):
        # The one that matters. Waiting here is waiting for a failure to stop
        # having happened.
        self.assertFalse(unsettled(pull(), [run(conclusion="failure")]))

    def test_a_green_run_is_settled(self):
        self.assertFalse(unsettled(pull(), [run()]))

    def test_a_conflicted_branch_is_settled_however_empty_its_run_list(self):
        # GitHub never evaluates the workflow's triggers for a branch it
        # cannot merge, so no run will ever appear. Waiting is waiting for
        # something that cannot happen, and the answer is a rebase.
        self.assertFalse(unsettled(pull(mergeable="CONFLICTING"), []))
        self.assertFalse(unsettled(pull(mergeStateStatus="DIRTY"), []))

    def test_a_draft_is_settled(self):
        # CI does not run on drafts, so there is nothing coming.
        self.assertFalse(unsettled(pull(isDraft=True), []))

    def test_mergeability_not_yet_computed_is_still_worth_waiting_for(self):
        # UNKNOWN means "ask again", which is exactly what waiting does.
        self.assertTrue(unsettled(pull(mergeable="UNKNOWN"), []))


class Waiting(unittest.TestCase):
    """`--wait`, driven against a scripted GitHub."""

    def drive(self, answers: list[tuple[dict, list[dict], dict]], *args: str):
        """Run `main` with `look` returning each answer in turn.

        The clock is fake as well as the sleeps, and that is not tidiness. A
        mocked `sleep` alone leaves `monotonic` frozen, so a bug that waits
        when it should not never reaches its deadline and the test **hangs**
        instead of failing. That happened here, while checking that this
        suite could catch exactly that bug: a hanging test is worse than a
        failing one, because nothing tells you which one it was.

        Sleeping advances the clock instead, so a loop that will not stop
        runs out of time and says so.
        """
        seen = {"polls": 0, "slept": []}
        now = [0.0]

        def look(_number):
            seen["polls"] += 1
            return answers[min(seen["polls"] - 1, len(answers) - 1)]

        def sleep(seconds):
            seen["slept"].append(seconds)
            now[0] += max(seconds, 1)

        original = (mergeable.look, mergeable.time.sleep, mergeable.time.monotonic)
        mergeable.look = look
        mergeable.time.sleep = sleep
        mergeable.time.monotonic = lambda: now[0]
        said = io.StringIO()
        try:
            with contextlib.redirect_stdout(said), contextlib.redirect_stderr(said):
                code = main(["mergeable.py", "12", *args])
        finally:
            mergeable.look, mergeable.time.sleep, mergeable.time.monotonic = original
        return code, said.getvalue(), seen

    def test_it_waits_for_a_run_to_appear_and_then_to_finish(self):
        code, said, seen = self.drive(
            [
                (pull(), [], {}),
                (pull(), [run(status="in_progress", conclusion=None)], jobs()),
                (pull(), [run()], jobs()),
            ],
            "--wait",
        )

        self.assertEqual(code, 0, said)
        self.assertEqual(seen["polls"], 3, "it should have asked three times")
        self.assertIn("mergeable: CI ran on", said)

    def test_it_stops_on_a_red_run_rather_than_waiting_out_the_timeout(self):
        # The failure mode of every hand-rolled loop this replaces: a naive
        # `until` over one exit code sits on a red pull request forever.
        code, said, seen = self.drive(
            [(pull(), [run(conclusion="failure")], jobs())], "--wait"
        )

        self.assertEqual(code, 1)
        self.assertEqual(seen["polls"], 1, "asking again would say the same")
        self.assertEqual(seen["slept"], [], "and it must not sleep first")

    def test_a_conflicted_branch_is_refused_at_once_with_the_rebase(self):
        code, said, seen = self.drive(
            [(pull(mergeable="CONFLICTING"), [], {})], "--wait"
        )

        self.assertEqual(code, 1)
        self.assertEqual(seen["polls"], 1)
        self.assertIn("Rebase", said)

    def test_without_wait_a_run_still_going_is_its_own_exit_code(self):
        # `1` and `3` are both "not mergeable". They are separate so that a
        # caller can tell "not yet" from "no" — which is the whole bug.
        code, _, seen = self.drive(
            [(pull(), [run(status="in_progress", conclusion=None)], jobs())]
        )

        self.assertEqual(code, 3)
        self.assertEqual(seen["polls"], 1, "no --wait means no waiting")

    def test_without_wait_a_red_run_is_the_other_code(self):
        code, _, _ = self.drive([(pull(), [run(conclusion="failure")], jobs())])
        self.assertEqual(code, 1)

    def test_a_wait_that_never_settles_gives_up_rather_than_running_forever(self):
        # With a real clock this is the difference between a test that fails
        # and a test that hangs. The loop must be bounded by the timeout, not
        # by whether the answer ever changes.
        code, said, seen = self.drive(
            [(pull(), [run(status="queued", conclusion=None)], jobs())],
            "--wait",
            "--timeout=60",
        )

        self.assertEqual(code, 3)
        self.assertIn("gave up waiting", said)
        self.assertGreater(seen["polls"], 1, "it did wait")
        self.assertLess(seen["polls"], 20, "but not forever")

    def test_giving_up_says_so_rather_than_claiming_a_verdict(self):
        # A timeout is not an answer about the pull request. It must not be
        # mistaken for one, and it must never hang forever.
        code, said, seen = self.drive(
            [(pull(), [run(status="queued", conclusion=None)], jobs())],
            "--wait",
            "--timeout=0",
        )

        self.assertEqual(code, 3)
        self.assertIn("gave up waiting", said)
        self.assertEqual(seen["polls"], 1)

    def test_a_timeout_that_is_not_a_number_is_a_usage_error(self):
        code, _, _ = self.drive([(pull(), [run()], jobs())], "--timeout=soon")
        self.assertEqual(code, 2)

    def test_an_unknown_flag_is_refused_rather_than_ignored(self):
        # Silently ignoring `--waitt` would wait for nothing and merge.
        code, _, _ = self.drive([(pull(), [run()], jobs())], "--waitt")
        self.assertEqual(code, 2)


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
