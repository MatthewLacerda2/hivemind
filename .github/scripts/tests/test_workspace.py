"""The two ways a machine can make a green run mean nothing.

Both judgements are pure, so none of this touches a disk or an environment —
which is also the only way to test "the disk is nearly full" on a machine that
is not.
"""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from workspace import GB, low_disk, shared_target  # noqa: E402


class SharedTarget(unittest.TestCase):
    """`CARGO_TARGET_DIR`, the one setting that can make every gate lie."""

    def test_an_unset_variable_is_the_arrangement_that_works(self):
        self.assertIsNone(shared_target({}))

    def test_a_shared_target_is_refused_and_says_where_it_points(self):
        why = shared_target({"CARGO_TARGET_DIR": "/tmp/shared"})
        self.assertIsNotNone(why)
        self.assertIn("/tmp/shared", why[0])

    def test_the_reason_given_is_the_false_green_not_the_wasted_hour(self):
        # Both happen, and only one of them is why this is a gate rather than
        # a note in CLAUDE.md. Somebody reading the refusal has to be told
        # which, or they will route around it the first time it is in the way.
        why = shared_target({"CARGO_TARGET_DIR": "/tmp/shared"})
        self.assertTrue(
            any("green" in line for line in why),
            f"the refusal must name the false green: {why}",
        )

    def test_an_empty_value_is_not_set(self):
        # `CARGO_TARGET_DIR=` is how a shell unsets it, and cargo reads it
        # that way too. Refusing here would refuse the fix for the refusal.
        self.assertIsNone(shared_target({"CARGO_TARGET_DIR": ""}))
        self.assertIsNone(shared_target({"CARGO_TARGET_DIR": "   "}))


class LowDisk(unittest.TestCase):
    """Running out mid-build never says `no space left on device` (#61)."""

    def test_plenty_of_room_says_nothing(self):
        self.assertIsNone(low_disk(200 * GB))

    def test_the_floor_itself_is_enough(self):
        self.assertIsNone(low_disk(10 * GB, floor_gb=10))

    def test_just_under_the_floor_is_worth_saying(self):
        why = low_disk(9 * GB, floor_gb=10)
        self.assertIsNotNone(why)
        self.assertIn("9.0 GB free", why[0])

    def test_it_says_what_to_do_rather_than_only_what_is_wrong(self):
        # A warning that leaves somebody at "yes, I know" is one they learn
        # to scroll past.
        why = low_disk(1 * GB)
        self.assertTrue(
            any("reap" in line for line in why),
            f"it should name the recipe that frees space: {why}",
        )

    def test_the_symptom_is_named_because_that_is_what_they_will_search_for(self):
        # The half hour this cost went on the error text, not on the disk.
        why = low_disk(1 * GB)
        self.assertTrue(
            any("went wrong on this node" in line for line in why),
            f"it should name what running out actually looks like: {why}",
        )


if __name__ == "__main__":
    unittest.main()
