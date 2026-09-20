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


class Prose(unittest.TestCase):
    """The same claim written as a sentence, which cost four milestones (#60)."""

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

    def test_the_comment_that_made_list_peers_lie_for_four_milestones(self):
        # Verbatim from #60. It sat above `Ok(Json(Vec::new()))` from M2 to
        # M7, and the MCP tool returned an empty list the whole time.
        self.write(
            "crates/hivemind-mcp/src/tools.rs",
            "    async fn list_peers(&self) -> Result<Json<Vec<PeerInfo>>, McpError> {\n"
            "        // Pairing and the peer book arrive in M3 (SPEC §14). Until then this is\n"
            "        // truthfully empty rather than absent: the tool surface is the contract.\n"
            "        Ok(Json(Vec::new()))\n"
            "    }\n",
        )
        found = stale_markers.stale({"M1", "M2", "M3"})
        self.assertEqual(len(found), 1, found)
        self.assertIn("tools.rs:2", found[0])

    def test_a_spec_reference_describing_the_past_is_fine(self):
        # Naming a shipped milestone is how a comment says where something
        # came from. Only a promise about it is stale.
        self.write(
            "crates/hivemind-mcp/src/tools.rs",
            "// Pairing and the peer book landed in M3 (SPEC §14), so this reads them.",
        )
        self.assertEqual(stale_markers.stale({"M1", "M2", "M3"}), [])

    def test_a_promise_about_work_still_to_come_is_fine(self):
        self.write(
            "crates/hivemind-api/src/service.rs",
            "// Relaying through a third node arrives in M9 (SPEC §14).",
        )
        self.assertEqual(stale_markers.stale({"M1", "M2", "M3"}), [])

    def test_portuguese_carries_the_same_promise(self):
        for sentence in ("// O livro de pares chega no M3.", "// Até lá, M3 não existe."):
            with self.subTest(sentence=sentence):
                self.write("crates/hivemind-api/src/service.rs", sentence)
                self.assertEqual(len(stale_markers.stale({"M3"})), 1)

    def test_a_milestone_under_the_last_shipped_one_counts(self):
        # `n` at or below what shipped, not membership of a set: a history
        # that records M3 and not M2 still means M2 happened.
        self.write("crates/hivemind-api/src/service.rs", "// Attachments arrive in M2.")
        self.assertEqual(len(stale_markers.stale({"M3"})), 1)

    def test_prose_outside_a_comment_is_the_roadmap_describing_itself(self):
        # SPEC §14 is a list of milestones and says what each one brings. It
        # is not a promise left behind in code, and a gate that fired on it
        # would be one nobody could keep green.
        self.write("SPEC.md", "M3 — peers. Pairing and the peer book arrive here.")
        self.assertEqual(stale_markers.stale({"M3"}), [])

    def test_a_milestone_in_code_rather_than_a_comment_is_not_prose(self):
        self.write(
            "crates/hivemind-core/src/node.rs",
            'let m3_arrives = format!("arrives in M3");',
        )
        self.assertEqual(stale_markers.stale({"M3"}), [])

    def test_one_line_is_reported_once_however_many_rules_fire(self):
        self.write("crates/hivemind-api/src/service.rs", "// TODO(M3): peers arrive in M3.")
        self.assertEqual(len(stale_markers.stale({"M3"})), 1)


if __name__ == "__main__":
    unittest.main()
