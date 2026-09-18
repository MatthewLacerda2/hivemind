"""The stale-marker rule, including the distinction that needed finding.

`unittest` so the gate needs nothing installed.
"""

import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import stale_markers  # noqa: E402


class Markers(unittest.TestCase):
    def setUp(self):
        self.tree = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tree.name)
        real = stale_markers.ROOT
        stale_markers.ROOT = self.root
        self.addCleanup(self.tree.cleanup)
        self.addCleanup(setattr, stale_markers, "ROOT", real)

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def test_a_marker_for_a_shipped_milestone_is_stale(self):
        self.write("README.md", "<!-- TODO(M1): the table -->")
        found = stale_markers.stale({"M1"})
        self.assertEqual(len(found), 1, found)
        self.assertIn("README.md:1", found[0])

    def test_a_marker_for_future_work_is_fine(self):
        self.write("README.md", "<!-- TODO(M9): a thing not done yet -->")
        self.assertEqual(stale_markers.stale({"M1"}), [])

    def test_a_bare_todo_has_no_expiry_and_is_not_checked(self):
        # This says nothing about markers with no milestone; that is a
        # different argument, and silently taking a side on it would be worse
        # than leaving it alone.
        self.write("README.md", "// TODO: someday")
        self.assertEqual(stale_markers.stale({"M1"}), [])

    def test_prose_about_a_marker_is_not_a_marker(self):
        # Found by this gate catching the two comments written to explain what
        # it had just found. Real markers are never backticked — nothing
        # renders them.
        self.write(
            "crates/hivemind-api/src/problem.rs",
            "// The table used to carry a `TODO(M1)` instead.",
        )
        self.assertEqual(stale_markers.stale({"M1"}), [])

    def test_fixme_and_xxx_count_too(self):
        for word in ("FIXME", "XXX"):
            with self.subTest(word=word):
                self.write("README.md", f"<!-- {word}(M2): x -->")
                self.assertEqual(len(stale_markers.stale({"M2"})), 1)

    def test_generated_and_lock_files_are_somebody_elses_markers(self):
        self.write("crates/Cargo.lock", "# TODO(M1): not ours")
        self.assertEqual(stale_markers.stale({"M1"}), [])


if __name__ == "__main__":
    unittest.main()
