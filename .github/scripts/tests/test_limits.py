"""Which files have a cap, what counts, and what a skill's header must carry.

Both directions are tested: a file over its cap must fail, and a file at it
must not, because an off-by-one either way is a gate that lies.
"""

import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from limits import (  # noqa: E402
    CLAUDE_MD_LINES,
    DESCRIPTION_CHARS,
    NAME_CHARS,
    SCRIPT_LINES,
    SKILL_MD_LINES,
    check,
    frontmatter,
    read_files,
)


def lines(n: int) -> str:
    return "".join(f"line {i}\n" for i in range(n))


def skill(name: str = "a-skill", description: str = "Does a thing.") -> str:
    return f"---\nname: {name}\ndescription: {description}\n---\n\n# Body\n"


class Caps(unittest.TestCase):
    def assert_cap(self, path: str, cap: int) -> None:
        at, _ = check({path: lines(cap)})
        self.assertEqual(at, [], f"{path} at {cap} lines is within its cap")
        over, _ = check({path: lines(cap + 1)})
        self.assertEqual(len(over), 1, f"{path} at {cap + 1} lines is over")
        self.assertIn(f"{cap + 1} lines > {cap}", over[0])

    def test_every_claude_md_is_capped_wherever_it_lives(self):
        self.assert_cap("CLAUDE.md", CLAUDE_MD_LINES)
        self.assert_cap("crates/hivemind-core/CLAUDE.md", CLAUDE_MD_LINES)

    def test_every_markdown_file_in_a_skill_is_capped_not_only_skill_md(self):
        self.assert_cap(".claude/skills/x/reference/notes.md", SKILL_MD_LINES)

    def test_scripts_of_each_language_are_capped(self):
        for path in (".github/scripts/a.py", "web/src/main.ts", "tools/b.js"):
            self.assert_cap(path, SCRIPT_LINES)

    def test_prose_and_blank_lines_count_unlike_size_py(self):
        problems, _ = check({"CLAUDE.md": "\n" * (CLAUDE_MD_LINES + 1)})
        self.assertEqual(len(problems), 1, problems)

    def test_files_without_a_cap_are_ignored(self):
        problems, largest = check(
            {"README.md": lines(5000), "crates/x/src/lib.rs": lines(5000)}
        )
        self.assertEqual((problems, largest), ([], []))

    def test_the_generated_bundle_is_held_to_its_source_not_to_a_cap(self):
        problems, _ = check({"crates/hivemind-api/assets/hivemind.js": lines(9000)})
        self.assertEqual(problems, [])

    def test_a_pass_reports_the_largest_of_each_kind(self):
        _, largest = check({"CLAUDE.md": lines(10), "a/CLAUDE.md": lines(20)})
        self.assertEqual([(m.path, m.lines) for m in largest], [("a/CLAUDE.md", 20)])


class SkillHeader(unittest.TestCase):
    PATH = ".claude/skills/a-skill/SKILL.md"

    def problems(self, text: str) -> list[str]:
        return check({self.PATH: text})[0]

    def test_a_well_formed_header_passes(self):
        self.assertEqual(self.problems(skill()), [])

    def test_the_name_may_be_64_characters_and_not_65(self):
        self.assertEqual(self.problems(skill(name="n" * NAME_CHARS)), [])
        (problem,) = self.problems(skill(name="n" * (NAME_CHARS + 1)))
        self.assertIn(f"`name` is {NAME_CHARS + 1} characters", problem)

    def test_the_description_may_be_1024_characters_and_not_1025(self):
        self.assertEqual(self.problems(skill(description="d" * DESCRIPTION_CHARS)), [])
        (problem,) = self.problems(skill(description="d" * (DESCRIPTION_CHARS + 1)))
        self.assertIn(f"`description` is {DESCRIPTION_CHARS + 1} characters", problem)

    def test_a_missing_field_or_header_is_a_problem_not_a_pass(self):
        (problem,) = self.problems("---\nname: a-skill\n---\n")
        self.assertIn("`description` is missing", problem)
        (problem,) = self.problems("# No header at all\n")
        self.assertIn("no `---` header", problem)
        (problem,) = self.problems("---\nname: a-skill\n")
        self.assertIn("no `---` header", problem, "a header never closed")

    def test_only_skill_md_needs_a_header(self):
        self.assertEqual(check({".claude/skills/a-skill/notes.md": "# Notes\n"})[0], [])


class Frontmatter(unittest.TestCase):
    def test_quotes_are_not_part_of_the_value(self):
        fields = frontmatter("---\nname: \"quoted\"\ndescription: 'single'\n---\n")
        self.assertEqual(fields, {"name": "quoted", "description": "single"})

    def test_a_block_scalar_is_measured_as_its_joined_lines(self):
        # Measured as the block marker alone, a 2000-character description
        # would read as one character long.
        text = "---\nname: x\ndescription: >\n  first half\n  second half\n---\n"
        self.assertEqual(frontmatter(text)["description"], "first half second half")


class TheRealRepository(unittest.TestCase):
    def test_the_search_finds_this_repositorys_own_files(self):
        # An empty search passes every cap. This is what tells them apart.
        files = read_files()
        self.assertIn("CLAUDE.md", files)
        self.assertIn(".claude/skills/ci-merge/SKILL.md", files)
        self.assertIn(".github/scripts/limits.py", files)
        self.assertNotIn("Cargo.toml", files, "only files with a cap are read")

    def test_a_file_not_yet_added_is_checked_and_an_ignored_one_is_not(self):
        # Held to its cap before anybody remembers to `git add` it, and never
        # for what `target/` or `node_modules/` happen to hold.
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            (root / ".gitignore").write_text("ignored/\n")
            (root / "CLAUDE.md").write_text("new\n")
            (root / "ignored").mkdir()
            (root / "ignored" / "big.js").write_text("x\n")
            files = read_files(root)
        self.assertIn("CLAUDE.md", files)
        self.assertNotIn("ignored/big.js", files)


if __name__ == "__main__":
    unittest.main()
