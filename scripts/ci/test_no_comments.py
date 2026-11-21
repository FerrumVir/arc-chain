from __future__ import annotations

import hashlib
import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parent.parent
SCRIPT = str(HERE / "no_comments.py")
SPEC = importlib.util.spec_from_file_location("no_comments", HERE / "no_comments.py")
nc = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = nc
SPEC.loader.exec_module(nc)

WORKFLOW = ".github/workflows/x.yml"
STEPS = "on: push\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n"


def flagged(path: str, text: str) -> list[str]:
    return sorted(text[start:end] for _, start, end in nc.disallowed_comments(path, text))


def self_reads(path: str, text: str) -> list[str]:
    return [text[start:end] for start, end in nc.self_reading_commands(path, text)]


def run_checker(root: str, *arguments: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, SCRIPT, "--root", root, *arguments],
        capture_output=True,
        text=True,
        check=False,
    )


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
            "use clap::Parser;\n"
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

    def test_inner_module_docs_are_never_clap_help(self) -> None:
        text = (
            "use clap::Parser;\n"
            "//! Module prose that is not help text.\n"
            "#[derive(Parser)]\n"
            "struct Cli {\n"
            "    /// Listen port.\n"
            "    port: u16,\n"
            "}\n"
        )
        self.assertEqual(flagged("src/main.rs", text), ["//! Module prose that is not help text."])

    def test_parser_derive_is_clap_help_only_when_clap_is_used(self) -> None:
        pest = (
            "use pest_derive::Parser;\n"
            "/// Grammar parser prose.\n"
            "#[derive(Parser)]\n"
            '#[grammar = "g.pest"]\n'
            "struct Grammar;\n"
        )
        self.assertEqual(flagged("src/grammar.rs", pest), ["/// Grammar parser prose."])
        qualified = "/// Run the tool.\n#[derive(clap::Parser)]\nstruct Cli;\n"
        self.assertEqual(flagged("src/main.rs", qualified), [])


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

    def test_shebang_is_allowed_only_on_line_one(self) -> None:
        self.assertEqual(
            flagged("tool.py", "#!/usr/bin/env python3\nx = 1\n#! prose that explains a hack\n"),
            ["#! prose that explains a hack"],
        )

    def test_noqa_must_start_the_comment(self) -> None:
        text = "x = 1  # we do this because of reasons  # noqa\ny = 2  # noqa: E501\n"
        self.assertEqual(flagged("tool.py", text), ["# we do this because of reasons  # noqa"])

    def test_type_comment_must_be_ignore_or_a_type(self) -> None:
        text = (
            "a = []  # type: List[int]\n"
            "b = f()  # type: ignore[attr-defined]\n"
            "c = 1  # type: because the API is loose\n"
            "d = 2  # type: ignore  # flaky stub\n"
            "e = 3  # type: not used\n"
            "f = g()  # type: Callable[[int], str] | None\n"
            "def g(x):\n"
            "    # type: (int) -> str\n"
            "    return str(x)\n"
        )
        self.assertEqual(
            flagged("tool.py", text),
            ["# type: because the API is loose", "# type: ignore  # flaky stub", "# type: not used"],
        )

    def test_coding_declaration_is_allowed_only_on_lines_one_and_two(self) -> None:
        self.assertEqual(
            flagged("tool.py", "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\nx = 1\n"), []
        )
        self.assertEqual(flagged("tool.py", "# coding=latin-1\nx = 1\n"), [])
        self.assertEqual(flagged("tool.py", "\n# vim: set fileencoding=utf-8 :\nx = 1\n"), [])
        self.assertEqual(flagged("tool.py", "x = 1\n# coding: utf-8\n"), ["# coding: utf-8"])
        self.assertEqual(
            flagged("tool.py", "# Note on coding: we prefer x\nx = 1\n"),
            ["# Note on coding: we prefer x"],
        )
        self.assertEqual(flagged("tool.py", "x = 1\ny = 2\n# coding: utf-8\n"), ["# coding: utf-8"])
        self.assertEqual(flagged("tool.py", "# coding: no-such-codec\n"), ["# coding: no-such-codec"])

    def test_unparseable_python_raises_unreadable_source(self) -> None:
        for text in ("x = 1\x00\n", "def (:\n"):
            with self.subTest(text=text):
                with self.assertRaises(nc.UnreadableSource):
                    nc.disallowed_comments("tool.py", text)


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

    def test_heredoc_is_python_only_when_it_is_the_program(self) -> None:
        text = (
            "python3 tool.py <<'EOF'\n"
            "# heading the tool reads\n"
            "EOF\n"
            "cat > \"$bin/python3\" <<'PY'\n"
            "# shim text\n"
            "PY\n"
            "python3 -I - \"$1\" <<'PY'\n"
            "x = 1  # program comment\n"
            "PY\n"
            "ARC=1 python3 <<'PY'\n"
            "y = 2  # stdin program\n"
            "PY\n"
            "python3 2>/dev/null - <<'PY'\n"
            "z = 3  # after a redirection\n"
            "PY\n"
        )
        self.assertEqual(
            flagged("run.sh", text), ["# after a redirection", "# program comment", "# stdin program"]
        )

    def test_python_heredoc_that_cannot_parse_is_unreadable(self) -> None:
        with self.assertRaises(nc.UnreadableSource) as raised:
            nc.disallowed_comments("run.sh", "echo start\npython3 - <<'PY'\ndef (:\nPY\n")
        self.assertEqual(raised.exception.offset, len("echo start\npython3 - <<'PY'\n"))

    def test_hash_after_a_regex_group_or_extglob_is_pattern_text(self) -> None:
        text = (
            'if [[ "$msg" =~ (^|[[:space:]])#([0-9]+) ]]; then echo; fi\n'
            "[[ $x =~ ^(a|b)#$ ]] && echo y\n"
            "shopt -s extglob\n"
            "case $x in @(a|b)#*) echo ;; esac\n"
        )
        self.assertEqual(flagged("run.sh", text), [])

    def test_hash_after_a_subshell_or_case_pattern_is_a_comment(self) -> None:
        text = "( cd /tmp )  # x\n(echo a)# y\ncase $x in\n  a) # z\n    echo ;;\nesac\n"
        self.assertEqual(flagged("run.sh", text), ["# x", "# y", "# z"])

    def test_hash_after_a_line_continuation_at_a_word_break_is_a_comment(self) -> None:
        self.assertEqual(flagged("run.sh", "echo a \\\n#b\necho c\\\n#d\n"), ["#b"])

    def test_shebang_is_allowed_only_on_line_one(self) -> None:
        text = "#!/bin/bash\necho\n#! this is why we retry\npython3 - <<'PY'\n#!/usr/bin/env python3\nPY\n"
        self.assertEqual(flagged("run.sh", text), ["#! this is why we retry", "#!/usr/bin/env python3"])


class SelfReadingScriptTests(unittest.TestCase):
    def test_commands_that_print_the_script_itself_are_flagged(self) -> None:
        text = (
            "usage() {\n"
            "  sed -n '2,17p' \"$0\" | sed 's/^# //'\n"
            "}\n"
            "case \"$1\" in\n"
            "  -h | --help) sed -n '2,19p' \"$0\"; exit 0 ;;\n"
            "esac\n"
            "awk 'NR > 1 { print; next }' \"${BASH_SOURCE[0]}\"\n"
            "if grep -q marker ${0}; then :; fi\n"
            "head -n 3 $0\n"
            "cat < \"$0\"\n"
        )
        self.assertEqual(
            self_reads("run.sh", text),
            [
                "sed -n '2,17p' \"$0\"",
                "sed -n '2,19p' \"$0\"",
                "awk 'NR > 1 { print; next }' \"${BASH_SOURCE[0]}\"",
                "grep -q marker ${0}",
                "head -n 3 $0",
                "cat < \"$0\"",
            ],
        )

    def test_uses_of_the_script_path_are_not_flagged(self) -> None:
        text = (
            'here="$(dirname "$0")"\n'
            'real="$(realpath "$0")"\n'
            'link="$(readlink -f "${BASH_SOURCE[0]}")"\n'
            'cat "$(dirname "$0")/VERSION"\n'
            'echo "$0"\n'
            'cat <<< "$0"\n'
            "cat <<'EOF'\n"
            "sed -n 1p \"$0\"\n"
            "EOF\n"
        )
        self.assertEqual(self_reads("run.sh", text), [])
        self.assertEqual(self_reads("tool.py", "sed = '$0'\n"), [])

    def test_command_line_names_the_heredoc_fix(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            Path(root, "run.sh").write_text("sed -n '2,5p' \"$0\"\n", encoding="utf-8")
            result = run_checker(root, "run.sh")
        self.assertEqual(result.returncode, 1)
        self.assertIn("run.sh:1:1: reads its own source: sed -n '2,5p' \"$0\"", result.stdout)
        self.assertIn("cat <<'EOF'", result.stderr)
        self.assertNotIn("comment:", result.stdout)


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
            flagged(WORKFLOW, text),
            sorted(["# top comment", "# step comment", "# step line", "# ps comment"]),
        )

    def test_cloud_init_header_is_kept(self) -> None:
        self.assertEqual(flagged("deploy/cloud-init.yml", "#cloud-config\nfinal_message: done\n"), [])

    def test_python_step_with_expressions_is_read(self) -> None:
        text = STEPS + (
            "      - shell: python\n"
            "        run: |\n"
            "          n = ${{ inputs.count }}  # why\n"
            "          print(n)\n"
        )
        self.assertEqual(flagged(WORKFLOW, text), ["# why"])

    def test_unparseable_python_step_is_unreadable_at_its_line(self) -> None:
        text = STEPS + "      - shell: python\n        run: |\n          print(1)\n          def (:\n"
        findings = nc.file_findings(WORKFLOW, text)
        self.assertEqual([(f.line, f.rule) for f in findings], [(9, nc.UNREADABLE)])

    def test_step_shell_picks_the_lexer(self) -> None:
        cmd = STEPS + (
            "      - shell: cmd\n"
            "        run: |\n"
            "          echo #hashtag\n"
            "          REM note\n"
            "          echo x & rem tail\n"
        )
        self.assertEqual(flagged(WORKFLOW, cmd), ["REM note", "rem tail"])
        node = STEPS + "      - shell: node {0}\n        run: |\n          console.log(`# Title`) // c\n"
        self.assertEqual(flagged(WORKFLOW, node), ["// c"])

    def test_job_and_workflow_defaults_pick_the_lexer(self) -> None:
        text = (
            "on: push\n"
            "defaults:\n"
            "  run:\n"
            "    shell: python\n"
            "jobs:\n"
            "  win:\n"
            "    runs-on: windows-latest\n"
            "    defaults:\n"
            "      run:\n"
            "        shell: pwsh\n"
            "    steps:\n"
            "      - run: |\n"
            "          <# note #>\n"
            "  lin:\n"
            "    runs-on: ubuntu-24.04\n"
            "    steps:\n"
            "      - run: |\n"
            '          x = "$("  # py\n'
            "      - shell: bash\n"
            "        run: |\n"
            "          echo $# # sh\n"
        )
        self.assertEqual(flagged(WORKFLOW, text), ["# py", "# sh", "<# note #>"])

    def test_github_script_blocks_are_read_as_javascript(self) -> None:
        text = STEPS + (
            "      - uses: actions/github-script@60a0d83039c74a4aee543508d2ffcb1c3799cdea\n"
            "        with:\n"
            "          script: |\n"
            "            // js comment\n"
            "            core.info('# not a comment')\n"
            "      - uses: other/action@v1\n"
            "        with:\n"
            "          script: |\n"
            "            // text for another action\n"
        )
        self.assertEqual(flagged(WORKFLOW, text), ["// js comment"])

    def test_shebang_inside_a_run_block_is_a_comment(self) -> None:
        text = STEPS + "      - run: |\n          #!/bin/bash\n          echo hi\n"
        self.assertEqual(flagged(WORKFLOW, text), ["#!/bin/bash"])


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

    def test_jsx_in_js_files_is_read_as_jsx(self) -> None:
        self.assertEqual(flagged("b.js", "const a = <p>// text</p>; // c\n"), ["// c"])

    def test_tool_directives_are_kept(self) -> None:
        text = (
            "// @ts-expect-error legacy value\n"
            "const x: number = \"a\";\n"
            "/* eslint-disable no-console */\n"
        )
        self.assertEqual(flagged("a.ts", text), [])

    def test_keyword_after_a_dot_is_a_property_name(self) -> None:
        text = "const y = obj.in / 2; const u = 'a//b'; // c\n"
        self.assertEqual(flagged("a.js", text), ["// c"])

    def test_generic_arrow_with_a_default_type_is_not_jsx(self) -> None:
        self.assertEqual(flagged("A.tsx", "const f = <T = string>(x: T) => x; // c\n"), ["// c"])

    def test_regex_after_a_control_statement_condition(self) -> None:
        text = "if (ok) /'/.test(s);\nconst u = 'http://x'; // c\nconst v = f(a) / 2; // d\n"
        self.assertEqual(flagged("a.js", text), ["// c", "// d"])

    def test_license_banner_needs_license_text(self) -> None:
        text = (
            "/*! This explains our workaround */\n"
            "/*! tailwindcss v3.4.1 | MIT License | https://tailwindcss.com */\n"
            "/*! Copyright 2026 ARC */\n"
            "x = 1;\n"
        )
        self.assertEqual(flagged("a.js", text), ["/*! This explains our workaround */"])
        css = "/*! prose about the hack */ a{}\n/*! normalize.css v8 | MIT License */\n"
        self.assertEqual(flagged("a.css", css), ["/*! prose about the hack */"])

    def test_license_tags_must_start_the_comment(self) -> None:
        text = (
            "/* We retry here because the API flakes. @preserve */\n"
            "// see @license for why this is weird\n"
            "/** @license MIT */\n"
            "/*!\n * @preserve\n */\n"
        )
        self.assertEqual(
            flagged("a.js", text),
            ["/* We retry here because the API flakes. @preserve */", "// see @license for why this is weird"],
        )
        self.assertEqual(flagged("a.css", "/* prose @license */\n/* @license MIT */\n"), ["/* prose @license */"])


class DirectiveTests(unittest.TestCase):
    def test_gitleaks_allow_must_be_the_whole_comment(self) -> None:
        text = "const k = 'x'; // gitleaks:allow\nconst j = 'y'; // prose explaining stuff gitleaks:allow\n"
        self.assertEqual(flagged("a.js", text), ["// prose explaining stuff gitleaks:allow"])
        self.assertEqual(flagged("deploy/x.yml", "token: abc # gitleaks:allow\n"), [])
        self.assertEqual(flagged("tool.py", "token = 'abc'  # gitleaks:allow because\n"), ["# gitleaks:allow because"])


class PowerShellTests(unittest.TestCase):
    def test_comment_based_help_at_the_top_of_a_script_is_kept(self) -> None:
        text = "<#\n.SYNOPSIS\nDoes a thing.\n\n.EXAMPLE\npwsh ./a.ps1\n#>\nparam()\n# c\n"
        self.assertEqual(flagged("a.ps1", text), ["# c"])
        self.assertEqual(flagged("m.psm1", text), ["# c"])
        self.assertEqual(flagged("a.ps1", "\ufeff\n<# .DESCRIPTION\nRuns.\n#>\nparam()\n"), [])

    def test_other_block_comments_are_flagged(self) -> None:
        cases = {
            "<# block #>\n$a = 1\n": ["<# block #>"],
            "$a = 1\n<#\n.SYNOPSIS\nLate help.\n#>\n": ["<#\n.SYNOPSIS\nLate help.\n#>"],
            "#requires -Version 7\n<#\n.SYNOPSIS\nx\n#>\n": ["<#\n.SYNOPSIS\nx\n#>"],
            "function F {\n<#\n.SYNOPSIS\nDoes F.\n#>\n}\n": ["<#\n.SYNOPSIS\nDoes F.\n#>"],
        }
        for text, expected in cases.items():
            with self.subTest(text=text):
                self.assertEqual(flagged("a.ps1", text), expected)
        step = STEPS + "      - shell: pwsh\n        run: |\n          <#\n          .SYNOPSIS\n          Step.\n          #>\n"
        self.assertEqual([(f.line, f.detail) for f in nc.file_findings(WORKFLOW, step)], [(8, "<#")])

    def test_hash_inside_a_bare_word_or_nested_string_is_code(self) -> None:
        text = 'Write-Output build#42\nWrite-Output `#escaped\n"a $("b # x") c" # c\n'
        self.assertEqual(flagged("a.ps1", text), ["# c"])

    def test_shebang_is_allowed_only_on_line_one(self) -> None:
        text = "#!/usr/bin/env pwsh\nWrite-Output 1\n#! prose\n"
        self.assertEqual(flagged("a.ps1", text), ["#! prose"])


class MakeTests(unittest.TestCase):
    def test_hash_inside_a_variable_reference_is_literal(self) -> None:
        text = "X := $(shell echo '#')\nY := ${Z} # c\nall:\n\techo $$# # recipe\n"
        self.assertEqual(flagged("Makefile", text), ["# c", "# recipe"])

    def test_recipe_line_shebang_is_a_comment(self) -> None:
        self.assertEqual(flagged("Makefile", "all:\n\t#! prose\n\techo\n"), ["#! prose"])


class CFamilyTests(unittest.TestCase):
    def test_quote_character_literal_is_code(self) -> None:
        text = "char q = '\"';\nconst char *u = \"http://x\"; // c\nint n = 1'000; // d\n"
        for path in ("k.metal", "k.cu", "k.c", "k.h"):
            with self.subTest(path=path):
                self.assertEqual(flagged(path, text), ["// c", "// d"])


class OtherLanguageTests(unittest.TestCase):
    def test_hash_inside_toml_multiline_strings_is_text(self) -> None:
        text = (
            'reason = """\n'
            "RUSTSEC-2024-0001 stays open until crypto-bigint#1358 ships.\n"
            '"""\n'
            "note = '''\n"
            "# literal text\n"
            "'''\n"
            "ignore = [] # c\n"
        )
        self.assertEqual(flagged("deny.toml", text), ["# c"])

    def test_markers_inside_textarea_and_attributes_are_text(self) -> None:
        text = (
            "<textarea><!-- user text --></textarea>\n"
            '<div title="<!-- x -->"></div>\n'
            "<!-- c -->\n"
            '<script>const u = "//x"; // js\n</script>\n'
        )
        self.assertEqual(flagged("a.html", text), ["// js", "<!-- c -->"])

    def test_zsh_and_timer_files_are_scanned(self) -> None:
        self.assertEqual(flagged("tool.zsh", "echo a # c\n"), ["# c"])
        self.assertEqual(flagged("deploy/x.timer", "[Timer]\n# c\nOnCalendar=daily\n"), ["# c"])


class ScopeTests(unittest.TestCase):
    def test_vendored_and_prose_paths_are_exempt(self) -> None:
        self.assertTrue(nc.is_exempt("vendor/stwo/src/lib.rs"))
        self.assertTrue(nc.is_exempt("docs/notes/example.sh"))
        self.assertFalse(nc.is_exempt("crates/arc-node/src/main.rs"))

    def test_fixture_code_is_scanned_but_fixture_data_is_not(self) -> None:
        self.assertFalse(nc.is_exempt("tests/release/fixtures/probe.sh"))
        self.assertFalse(nc.is_exempt("crates/x/testdata/gen.py"))
        self.assertTrue(nc.is_exempt("tests/release/fixtures/release.json"))
        self.assertTrue(nc.is_exempt("crates/x/fixtures/v1/SHA256SUMS"))
        self.assertTrue(nc.is_exempt("desktop/node_modules/a/index.js"))

    def test_sha256_pinned_files_are_still_pinned(self) -> None:
        for path in sorted(nc.SHA256_PINNED_FILES):
            with self.subTest(path=path):
                pinned = REPOSITORY / path
                self.assertTrue(
                    pinned.is_file(),
                    "%s is listed in SHA256_PINNED_FILES but does not exist; remove it from the list." % path,
                )
                digest = hashlib.sha256(pinned.read_bytes()).hexdigest()
                search = subprocess.run(
                    ["git", "-C", str(REPOSITORY), "grep", "-l", "-F", digest],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertIn(search.returncode, (0, 1), search.stderr)
                self.assertTrue(
                    search.stdout.strip(),
                    "%s is exempt because a tracked file pins its SHA-256, but no tracked file "
                    "holds %s any more. Remove it from SHA256_PINNED_FILES so the checker scans "
                    "it, or restore the pin." % (path, digest),
                )

    def test_command_line_reports_each_comment_with_guidance(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            Path(root, "lib.rs").write_text("fn a() {} // why\n", encoding="utf-8")
            Path(root, "ok.rs").write_text("fn b() {}\n", encoding="utf-8")
            failing = run_checker(root, "lib.rs", "ok.rs")
            clean = run_checker(root, "ok.rs")
        self.assertEqual(failing.returncode, 1)
        self.assertIn("lib.rs:1:11: comment: // why", failing.stdout)
        self.assertIn("ARC code carries no comments.", failing.stderr)
        self.assertEqual(clean.returncode, 0, clean.stdout + clean.stderr)

    def test_missing_path_exits_2(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            result = run_checker(root, "nope.rs")
        self.assertEqual(result.returncode, 2)
        self.assertIn("no such file: nope.rs", result.stderr)

    def test_absolute_and_dotted_paths_are_repository_relative(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            Path(root, "vendor").mkdir()
            Path(root, "vendor", "lib.rs").write_text("fn a() {} // vendored\n", encoding="utf-8")
            Path(root, "lib.rs").write_text("fn b() {} // why\n", encoding="utf-8")
            exempt = run_checker(root, str(Path(root, "vendor", "lib.rs")), "./vendor/lib.rs")
            scanned = run_checker(root, str(Path(root, "lib.rs")))
        self.assertEqual(exempt.returncode, 0, exempt.stdout + exempt.stderr)
        self.assertEqual(scanned.returncode, 1)
        self.assertTrue(scanned.stdout.startswith("lib.rs:1:11: comment:"), scanned.stdout)

    def test_untracked_files_are_scanned_and_ignored_files_are_not(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            subprocess.run(["git", "-C", root, "init", "-q"], check=True)
            Path(root, ".gitignore").write_text("ignored.rs\n", encoding="utf-8")
            Path(root, "tracked.rs").write_text("fn a() {}\n", encoding="utf-8")
            Path(root, "new.rs").write_text("fn b() {} // new\n", encoding="utf-8")
            Path(root, "ignored.rs").write_text("fn c() {} // ignored\n", encoding="utf-8")
            subprocess.run(["git", "-C", root, "add", ".gitignore", "tracked.rs"], check=True)
            result = run_checker(root)
        self.assertEqual(result.returncode, 1)
        self.assertIn("new.rs:1:11: comment: // new", result.stdout)
        self.assertNotIn("ignored.rs", result.stdout)

    def test_unreadable_files_are_reported_as_unreadable(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            Path(root, "nul.py").write_text("x = 1\x00\n", encoding="utf-8")
            Path(root, "bad.py").write_text("def (:\n", encoding="utf-8")
            Path(root, "latin.sh").write_bytes(b"echo caf\xe9\n")
            result = run_checker(root, "nul.py", "bad.py", "latin.sh")
        self.assertEqual(result.returncode, 1)
        self.assertIn("nul.py:1:6: cannot be read: contains a NUL byte", result.stdout)
        self.assertIn("bad.py:1:1: cannot be read: Python %d.%d cannot parse it" % sys.version_info[:2], result.stdout)
        self.assertIn("latin.sh:1:1: cannot be read: is not UTF-8 text; save it as UTF-8", result.stdout)
        self.assertNotIn("comment:", result.stdout)
        self.assertNotIn("Traceback", result.stderr)
        self.assertIn("error: 3 file(s) could not be read.", result.stderr)


if __name__ == "__main__":
    unittest.main()
