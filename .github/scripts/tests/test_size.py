"""What counts as a line of code, and what is free.

The counter decides whether a file is over the limit, so getting it wrong
either splits files whose code was never the problem or lets real growth
through. Both directions are tested.
"""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from size import ROOT, count, declared_test_files, read_sources  # noqa: E402


class Counting(unittest.TestCase):
    def test_blank_lines_and_comments_are_free(self):
        # `missing_docs` is a merge gate and the house style explains *why*, so
        # a cap that counted prose would put those two rules in opposition.
        text = """
/// Doc comment.
//! Module comment.
// Ordinary comment.

fn one() {}
"""
        self.assertEqual(count(text, is_test_file=False).source, 1)

    def test_a_trailing_comment_does_not_make_a_line_free(self):
        text = "fn one() {} // still code\n"
        self.assertEqual(count(text, is_test_file=False).source, 1)

    def test_block_comments_are_free_including_their_contents(self):
        text = """/*
fn not_really_code() {}
*/
fn one() {}
"""
        self.assertEqual(count(text, is_test_file=False).source, 1)

    def test_a_one_line_block_comment_does_not_swallow_the_file(self):
        text = "/* aside */\nfn one() {}\nfn two() {}\n"
        self.assertEqual(count(text, is_test_file=False).source, 2)


class SourceAndTest(unittest.TestCase):
    def test_a_cfg_test_module_counts_as_test_code(self):
        # Most tests here live in the file they test. Counting them as source
        # would call a small module with a thorough test module a large file.
        text = """fn one() {}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
"""
        counted = count(text, is_test_file=False)
        self.assertEqual(counted.source, 1, "only fn one()")
        # The attribute line, the `mod` line, and everything to its close.
        self.assertEqual(counted.test, 7)

    def test_code_after_a_test_module_is_source_again(self):
        text = """fn one() {}

#[cfg(test)]
mod tests {
    fn helper() {}
}

fn two() {}
"""
        counted = count(text, is_test_file=False)
        self.assertEqual(counted.source, 2, "one() and two()")
        self.assertEqual(counted.test, 4, "the attribute, the mod, helper, the close")

    def test_nested_braces_inside_a_test_module_do_not_end_it_early(self):
        text = """#[cfg(test)]
mod tests {
    fn helper() {
        if true {
            let _ = 1;
        }
    }
}

fn after() {}
"""
        counted = count(text, is_test_file=False)
        self.assertEqual(counted.source, 1, "only after()")

    def test_an_integration_test_file_is_all_test_code(self):
        text = "fn helper() {}\n#[test]\nfn it_works() {}\n"
        counted = count(text, is_test_file=True)
        self.assertEqual(counted.source, 0)
        self.assertEqual(counted.test, 3)

    def test_a_cfg_test_module_declaration_does_not_leak_past_itself(self):
        # `#[cfg(test)] mod y;` opens no brace, so the attribute is spent on
        # the declaration. Leaving it pending made every later line test code.
        text = """#[cfg(test)]
mod page_tests;

fn after() {}
"""
        counted = count(text, is_test_file=False)
        self.assertEqual(counted.source, 1, "only after()")
        self.assertEqual(counted.test, 2, "the attribute and the declaration")


def at(relative: str) -> pathlib.Path:
    return ROOT / relative


class DeclaredTestModules(unittest.TestCase):
    """Which files are test code because of how their parent declares them.

    The `#[cfg(test)]` that makes a child file test code lives in the parent,
    so a rule that only reads the file itself counts the whole of it as source
    (#110). The evidence is the declaration; a name ending `_tests` is a
    convention, and a rule keyed on a convention stops being true quietly.
    """

    def test_a_cfg_test_child_module_is_test_code(self):
        sources = {
            at("crates/a/src/web.rs"): "#[cfg(test)]\nmod page_tests;\n",
            at("crates/a/src/web/page_tests.rs"): "#[test]\nfn it_works() {}\n",
        }
        self.assertEqual(
            declared_test_files(sources), {at("crates/a/src/web/page_tests.rs")}
        )

    def test_a_child_named_tests_but_declared_plainly_is_source(self):
        # `peer/hello.rs` and `peer/hello_tests.rs` are siblings in this repo.
        # The name says nothing; only the declaration does.
        sources = {
            at("crates/a/src/peer.rs"): "mod hello_tests;\n",
            at("crates/a/src/peer/hello_tests.rs"): "pub fn helper() {}\n",
        }
        self.assertEqual(declared_test_files(sources), set())

    def test_an_attribute_on_an_inline_module_does_not_reach_the_next_file(self):
        # A `#[cfg(test)] mod tests { … }` above a plain `mod real;` must not
        # hand its attribute on to the declaration that follows.
        sources = {
            at("crates/a/src/peer.rs"): "#[cfg(test)]\nmod tests {}\nmod hello;\n",
            at("crates/a/src/peer/hello.rs"): "pub fn helper() {}\n",
        }
        self.assertEqual(declared_test_files(sources), set())

    def test_a_crate_root_declares_its_siblings(self):
        # `mod y;` in `lib.rs`, `main.rs` or `mod.rs` means `y.rs` beside it,
        # not `lib/y.rs`.
        sources = {
            at("crates/a/src/lib.rs"): "#[cfg(test)]\nmod fixtures;\n",
            at("crates/a/src/fixtures.rs"): "pub fn fixture() {}\n",
        }
        self.assertEqual(declared_test_files(sources), {at("crates/a/src/fixtures.rs")})

    def test_a_child_written_as_a_folder_resolves_to_its_mod_rs(self):
        sources = {
            at("crates/a/src/peer.rs"): "#[cfg(test)]\npub(crate) mod cases;\n",
            at("crates/a/src/peer/cases/mod.rs"): "pub fn case() {}\n",
        }
        self.assertEqual(
            declared_test_files(sources), {at("crates/a/src/peer/cases/mod.rs")}
        )

    def test_a_module_of_a_test_module_is_test_code_too(self):
        # Inside a file that is already test-only, a plain `mod z;` needs no
        # `#[cfg(test)]` of its own — and gets none.
        sources = {
            at("crates/a/src/web.rs"): "#[cfg(test)]\nmod page_tests;\n",
            at("crates/a/src/web/page_tests.rs"): "mod fixtures;\n",
            at("crates/a/src/web/page_tests/fixtures.rs"): "pub fn page() {}\n",
        }
        self.assertEqual(
            declared_test_files(sources),
            {
                at("crates/a/src/web/page_tests.rs"),
                at("crates/a/src/web/page_tests/fixtures.rs"),
            },
        )

    def test_a_commented_out_declaration_is_not_one(self):
        sources = {
            at("crates/a/src/web.rs"): "#[cfg(test)]\n// mod page_tests;\n",
            at("crates/a/src/web/page_tests.rs"): "#[test]\nfn it_works() {}\n",
        }
        self.assertEqual(declared_test_files(sources), set())


class AgainstTheRepository(unittest.TestCase):
    def test_every_cfg_test_declaration_in_the_tree_resolves(self):
        """A declaration the resolver cannot find is a rule that has gone quiet.

        `boundaries.py`'s network rule was anchored as if it were scanning
        `Cargo.toml` and would never have fired. The same family of bug lives
        here: if the layout the resolver assumes stops being the layout, this
        test says so rather than the count drifting.
        """
        sources = read_sources()
        declared = declared_test_files(sources)
        parents = [
            path
            for path, text in sources.items()
            if "mod page_tests;" in text or "_tests;" in text
        ]
        self.assertTrue(parents, "this repo uses the child-test-module pattern")
        self.assertTrue(declared, "and at least one of them resolves")
        for path in declared:
            self.assertTrue(path.exists(), path)


if __name__ == "__main__":
    unittest.main()
