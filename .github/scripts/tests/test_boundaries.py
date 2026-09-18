"""The boundary rules, checked against a tree built for the purpose.

A linter that has never refused anything is a linter nobody knows works. Each
rule gets a file that breaks it and a file that does not, in a temporary tree,
so the tests do not depend on the repo staying clean.
"""

import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import boundaries  # noqa: E402


class Boundaries(unittest.TestCase):
    def setUp(self):
        self.tree = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tree.name)
        self._real_root = boundaries.ROOT
        boundaries.ROOT = self.root
        self.addCleanup(self.tree.cleanup)
        self.addCleanup(setattr, boundaries, "ROOT", self._real_root)

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def rule(self, name: str) -> boundaries.Rule:
        return next(r for r in boundaries.RULES if r.name == name)

    # --- SQL ------------------------------------------------------------

    def test_sql_in_the_index_is_allowed(self):
        self.write(
            "crates/hivemind-core/src/index.rs",
            'let mut statement = conn.prepare("SELECT id FROM messages")?;',
        )
        self.assertEqual(breaches := boundaries.breaches(self.rule("SQL lives in the index")), [], breaches)

    def test_sql_anywhere_else_is_refused(self):
        self.write(
            "crates/hivemind-api/src/service.rs",
            'let rows = conn.prepare("SELECT id FROM messages")?;',
        )
        found = boundaries.breaches(self.rule("SQL lives in the index"))
        self.assertEqual(len(found), 1, found)
        self.assertIn("service.rs:1", found[0])

    def test_rusqlite_anywhere_else_is_refused(self):
        self.write("crates/hivemind-cli/src/commands.rs", "use rusqlite::Connection;")
        found = boundaries.breaches(self.rule("SQL lives in the index"))
        self.assertEqual(len(found), 1, found)

    def test_a_comment_mentioning_sql_is_not_a_breach(self):
        # The rule is about code, not about the doc comment explaining it.
        self.write(
            "crates/hivemind-api/src/service.rs",
            "// The index runs a SELECT over messages; we never do it here.\n"
            "/* rusqlite lives in index.rs */\n",
        )
        self.assertEqual(boundaries.breaches(self.rule("SQL lives in the index")), [])

    # --- the core crate -------------------------------------------------

    def test_a_network_dependency_in_core_is_refused(self):
        rule = self.rule("the core crate does no network I/O")
        self.write("crates/hivemind-core/src/store.rs", "use tokio::fs;")
        found = boundaries.breaches(rule)
        self.assertEqual(len(found), 1, found)
        self.assertIn("store.rs:1", found[0])

    def test_core_without_network_is_allowed(self):
        rule = self.rule("the core crate does no network I/O")
        self.write("crates/hivemind-core/src/store.rs", "use std::fs;\nuse serde::Serialize;")
        self.assertEqual(boundaries.breaches(rule), [])

    def test_the_rule_does_not_reach_other_crates(self):
        # hivemind-net is supposed to use tokio.
        rule = self.rule("the core crate does no network I/O")
        self.write("crates/hivemind-net/src/client.rs", "use tokio::net::TcpStream;")
        self.assertEqual(boundaries.breaches(rule), [])

    # --- the CLI --------------------------------------------------------

    def test_the_cli_may_open_the_store_where_spec_allows(self):
        rule = self.rule("the CLI reaches the store only where SPEC §10 allows")
        self.write("crates/hivemind-cli/src/commands.rs", "let store = MailStore::open(p)?;")
        self.assertEqual(boundaries.breaches(rule), [])

    def test_the_cli_may_not_open_the_store_elsewhere(self):
        rule = self.rule("the CLI reaches the store only where SPEC §10 allows")
        self.write("crates/hivemind-cli/src/doctor.rs", "let store = MailStore::open(p)?;")
        found = boundaries.breaches(rule)
        self.assertEqual(len(found), 1, found)
        self.assertIn("doctor.rs:1", found[0])

    # --- the rule table itself -------------------------------------------

    def test_every_rule_is_scoped_and_explains_itself(self):
        # A rule with no `why` can only be obeyed or deleted, never re-judged.
        for rule in boundaries.RULES:
            with self.subTest(rule=rule.name):
                self.assertIn(rule.name, boundaries.SCOPE)
                self.assertGreater(len(rule.why), 40, "say why, not just what")


if __name__ == "__main__":
    unittest.main()
