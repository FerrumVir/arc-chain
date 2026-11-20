from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("no_comments", HERE / "no_comments.py")
nc = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = nc
SPEC.loader.exec_module(nc)


def flagged(path: str, text: str) -> list[str]:
    return sorted(text[start:end] for _, start, end in nc.disallowed_comments(path, text))


class RustTests(unittest.TestCase):
    def test_line_block_and_doc_comments_are_flagged(self) -> None:
        text = "fn a() {}\n// line\n/* outer /* nested */ still */\n/// doc\n//! inner\nfn b() {}\n"
        self.assertEqual(
            flagged("src/lib.rs", text),
            sorted(["// line", "/* outer /* nested */ still */", "/// doc", "//! inner"]),
        )

    def test_comment_markers_inside_literals_are_code(self) -> None:
        text = (
            'const A: &str = "http://x /* y */";\n'
            'const B: &str = r##"// raw "# still"##;\n'
            "const C: char = '/';\n"
            "const D: &[u8] = b\"//\";\n"
        )
        self.assertEqual(flagged("src/lib.rs", text), [])

    def test_clap_help_text_is_kept(self) -> None:
        text = (
            "/// Run the node.\n"
            "#[derive(Parser, Debug)]\n"
            "struct Cli {\n"
            "    /// Listen port.\n"
            "    #[arg(long)]\n"
            "    port: u16,\n"
            "}\n"
            "/// Not help text.\n"
            "struct Other;\n"
        )
        self.assertEqual(flagged("src/main.rs", text), ["/// Not help text."])


class PythonTests(unittest.TestCase):
    def test_comments_and_docstrings_are_flagged_and_directives_kept(self) -> None:
        text = (
            "#!/usr/bin/env python3\n"
            '"""Module doc."""\n'
            "import os  # noqa: F401\n"
            'x = "# not a comment"\n'
            "# a comment\n"
            "def f():\n"
            '    """Function doc."""\n'
            "    return 1\n"
        )
        self.assertEqual(
            flagged("tool.py", text),
            sorted(['"""Module doc."""', "# a comment", '"""Function doc."""']),
        )

    def test_module_docstring_read_through_doc_is_kept(self) -> None:
        text = '"""Usage: tool [options]"""\nimport sys\nprint(__doc__)\n'
        self.assertEqual(flagged("tool.py", text), [])


class ShellTests(unittest.TestCase):
    def test_comments_are_flagged_but_parameter_syntax_is_code(self) -> None:
        text = (
            "#!/usr/bin/env bash\n"
            "# shellcheck disable=SC2034\n"
            "echo \"$#\" \"${#name}\" '# quoted' # trailing\n"
            "# whole line\n"
        )
        self.assertEqual(flagged("run.sh", text), ["# trailing", "# whole line"])

    def test_only_quoted_python_heredocs_are_read_as_python(self) -> None:
        text = (
            "python3 - \"$1\" <<'PY'\n"
            "x = 1  # in python\n"
            "PY\n"
            "cat <<'EOF'\n"
            "# text that cat prints\n"
            "EOF\n"
            "python3 - <<PY\n"
            "y = 2  # expanded by the shell, left alone\n"
            "PY\n"
        )
        self.assertEqual(flagged("run.sh", text), ["# in python"])


class YamlTests(unittest.TestCase):
    def test_comments_and_run_block_comments_are_flagged(self) -> None:
        text = (
            "on: push\n"
            "# top comment\n"
            "jobs:\n"
            "  a:\n"
            "    runs-on: ubuntu-24.04\n"
            "    steps:\n"
            "      - uses: actions/checkout@abc # gitleaks:allow\n"
            "      - run: |\n"
            "          echo \"#not\" # step comment\n"
            "          # step line\n"
            "      - shell: pwsh\n"
            "        run: |\n"
            "          Write-Host 'x' # ps comment\n"
        )
        self.assertEqual(
            flagged(".github/workflows/x.yml", text),
            sorted(["# top comment", "# step comment", "# step line", "# ps comment"]),
        )

    def test_cloud_init_header_is_kept(self) -> None:
        self.assertEqual(flagged("deploy/cloud-init.yml", "#cloud-config\nfinal_message: done\n"), [])


class JavaScriptTests(unittest.TestCase):
    def test_markers_in_strings_templates_and_regex_are_code(self) -> None:
        text = (
            'const a = "//x";\n'
            "const b = `/* ${a} */`;\n"
            "const r = /\\/\\//g;\n"
            "// real\n"
            "/* block */\n"
        )
        self.assertEqual(flagged("app.js", text), sorted(["// real", "/* block */"]))

    def test_jsx_text_is_not_a_comment(self) -> None:
        text = (
            "export const A = () => (\n"
            "  <div>\n"
            "    {/* hidden */}\n"
            "    <span>// shown text</span>\n"
            "  </div>\n"
            ");\n"
        )
        self.assertEqual(flagged("A.tsx", text), ["/* hidden */"])

    def test_tool_directives_are_kept(self) -> None:
        text = (
            "// @ts-expect-error legacy value\n"
            "const x: number = \"a\";\n"
            "/* eslint-disable no-console */\n"
        )
        self.assertEqual(flagged("a.ts", text), [])


class ScopeTests(unittest.TestCase):
    def test_vendored_and_prose_paths_are_exempt(self) -> None:
        self.assertTrue(nc.is_exempt("vendor/stwo/src/lib.rs"))
        self.assertTrue(nc.is_exempt("docs/notes/example.sh"))
        self.assertFalse(nc.is_exempt("crates/arc-node/src/main.rs"))

    def test_command_line_reports_each_comment_with_guidance(self) -> None:
        script = str(HERE / "no_comments.py")
        with tempfile.TemporaryDirectory() as root:
            Path(root, "lib.rs").write_text("fn a() {} // why\n", encoding="utf-8")
            Path(root, "ok.rs").write_text("fn b() {}\n", encoding="utf-8")
            failing = subprocess.run(
                [sys.executable, script, "--root", root, "lib.rs", "ok.rs"],
                capture_output=True, text=True, check=False,
            )
            clean = subprocess.run(
                [sys.executable, script, "--root", root, "ok.rs"],
                capture_output=True, text=True, check=False,
            )
        self.assertEqual(failing.returncode, 1)
        self.assertIn("lib.rs:1:11: comment: // why", failing.stdout)
        self.assertIn("ARC code carries no comments.", failing.stderr)
        self.assertEqual(clean.returncode, 0, clean.stdout + clean.stderr)


if __name__ == "__main__":
    unittest.main()
