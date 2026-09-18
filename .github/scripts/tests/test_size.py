"""What counts as a line of code, and what is free.

The counter decides whether a file is over the limit, so getting it wrong
either splits files whose code was never the problem or lets real growth
through. Both directions are tested.
"""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from size import count  # noqa: E402


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


if __name__ == "__main__":
    unittest.main()
