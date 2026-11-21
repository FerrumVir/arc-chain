#!/usr/bin/env python3
import argparse
import ast
import bisect
import codecs
import collections
import functools
import io
import operator
import os
import re
import subprocess
import sys
import tokenize

THIRD_PARTY_PREFIXES = ("vendor/",)
PROSE_PREFIXES = ("docs/", "papers/")
DATA_PREFIXES = ("release/", "deploy/config/", "testnet/")
THIRD_PARTY_PATH_PARTS = ("/node_modules/",)
DATA_PATH_PARTS = ("/fixtures/", "/testdata/")
DATA_FILES = frozenset(
    (
        "genesis.toml",
        "testnet-seeds.txt",
        "desktop/src-tauri/resources/genesis.toml",
        "desktop/src-tauri/resources/testnet-seeds.txt",
    )
)
SHA256_PINNED_FILES = frozenset(
    (
        "dashboard/tailwind.css",
        "dashboard/test-contract.mjs",
        "explorer/test-live.mjs",
        "scripts/recovery/drive-account-identity.py",
        "scripts/recovery/legacy-late-fork-interlock.py",
        "scripts/recovery/normalize-legacy-wal.py",
    )
)
GENERATED_SUFFIXES = (".min.js", ".min.css", ".map")

EXTENSION_LANGUAGES = {
    ".rs": "rust",
    ".ts": "ts",
    ".mts": "ts",
    ".cts": "ts",
    ".tsx": "tsx",
    ".jsx": "jsx",
    ".js": "jsx",
    ".mjs": "js",
    ".cjs": "js",
    ".py": "python",
    ".sh": "shell",
    ".bash": "shell",
    ".zsh": "shell",
    ".yml": "yaml",
    ".yaml": "yaml",
    ".toml": "toml",
    ".css": "css",
    ".html": "html",
    ".sol": "solidity",
    ".c": "c",
    ".h": "c",
    ".metal": "c",
    ".cu": "c",
    ".wgsl": "wgsl",
    ".ps1": "powershell",
    ".psm1": "powershell",
    ".nsh": "nsis",
    ".service": "unit",
    ".timer": "unit",
    ".conf": "hash",
    ".plist": "xml",
}
NAME_LANGUAGES = {
    "Makefile": "make",
    ".gitignore": "hashline",
    ".dockerignore": "hashline",
    ".gitattributes": "hashline",
}

LICENSE_TAG_AT_START = r"^(?://+|/\*+!?)[\s*]*@(?:license|preserve)\b"
LICENSE_BANNER = (
    r"(?s)^/\*!.*?(?:SPDX-License-Identifier|@license\b|\b(?:Copyright|License|Licensed|LICENSE)\b)"
)
DIRECTIVES = {
    "rust": (),
    "js": (
        r"^///\s*<(reference|amd-module|amd-dependency)\b",
        r"@ts-(ignore|expect-error|nocheck|check)\b",
        r"\beslint-(disable|enable)",
        r"\bprettier-ignore\b",
        r"@vite-ignore\b",
        r"[#@]__(PURE|NO_SIDE_EFFECTS)__",
        r"\bwebpack[A-Z]\w*\s*:",
        r"\b(istanbul|c8|v8) ignore\b",
        r"@jsx(ImportSource|Runtime|Frag)?\b",
        r"^//[#@] source(Mapping)?URL=",
        LICENSE_TAG_AT_START,
        LICENSE_BANNER,
    ),
    "python": (
        r"^#\s*(pragma\b|fmt:|pylint:|mypy:|pyright:|ruff:|isort:)",
        r"^#\s*noqa\b",
        r"^#\s*type:\s*ignore(\[[^\]#]*\])?\s*$",
    ),
    "shell": (r"^#\s*shellcheck\s",),
    "yaml": (
        r"^#\s*(yaml-language-server|yamllint|zizmor|actionlint)\b",
        r"^#cloud-(config|boothook)\b",
        r"^##\s*template:\s*jinja\b",
    ),
    "toml": (r"^#:schema\b",),
    "css": (LICENSE_BANNER, r"stylelint-(disable|enable)", LICENSE_TAG_AT_START),
    "solidity": (
        r"SPDX-License-Identifier",
        r"\bsolhint-(disable|enable)",
        r"forgefmt:",
        r"@custom:oz-",
        r"slither-disable",
    ),
    "c": (),
    "wgsl": (),
    "html": (r"^<!--\[if\b",),
    "make": (),
    "powershell": (r"^#\s*requires\b",),
    "cmd": (),
    "nsis": (),
    "unit": (),
    "hash": (),
    "hashline": (),
    "xml": (),
}
DIRECTIVE_FAMILY = {"ts": "js", "tsx": "js", "jsx": "js"}
UNIVERSAL_DIRECTIVES = (
    r"^(?:#|//|/\*|<!--|;|::|[Rr][Ee][Mm]\s)\s*gitleaks:allow\s*(?:\*/|-->)?\s*$",
)
PYTHON_TYPE_COMMENT = re.compile(r"^#\s*type:(?P<annotation>[^#]*)$")
ENCODING_DECLARATION = re.compile(
    r"#\s*(?:-\*-\s*coding[:=]\s*(?P<emacs>[-\w.]+)\s*-\*-"
    r"|vim:\s*set\s+fileencoding=(?P<vim>[-\w.]+)\s*:"
    r"|coding[:=]\s*(?P<plain>[-\w.]+))\s*$"
)
TYPE_EXPRESSION_NODES = (
    ast.Expression,
    ast.FunctionType,
    ast.Name,
    ast.Attribute,
    ast.Subscript,
    ast.Tuple,
    ast.List,
    ast.Constant,
    ast.BinOp,
    ast.BitOr,
    ast.Load,
)
POWERSHELL_HELP_KEYWORDS = (".SYNOPSIS", ".DESCRIPTION")
POWERSHELL_OPERATOR_CHARS = frozenset(";|&(){}[],=+*/%!<>")

CLAP_DERIVE_NAME = r"(?:Parser|Args|Subcommand|ValueEnum)\b"
CLAP_PATH_DERIVE = re.compile(r"\bclap\s*::\s*" + CLAP_DERIVE_NAME)
BARE_CLAP_DERIVE = re.compile(r"(?<![\w:])" + CLAP_DERIVE_NAME)
JS_REGEX_KEYWORDS = frozenset(
    "return typeof instanceof in of new delete void throw case do else yield await".split()
)
JS_CONTROL_KEYWORDS = frozenset(("if", "while", "for", "with"))
SHELL_WORD_BREAKS = " \t\r\n;&|()<>"
SHELL_REDIRECTION = re.compile(r"^(?:\d+|&)?[<>][<>&|]*$")
SHELL_INPUT_REDIRECTION = re.compile(r"^\d*<$")
SHELL_ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?=")
SHELL_COMMAND_PREFIXES = frozenset("! { if then elif else do while until time command exec builtin".split())
SELF_READING_TOOLS = frozenset(("sed", "head", "tail", "awk", "grep", "cat", "cut"))
SELF_REFERENCE = re.compile(
    r'^"?\$(?:0|\{0\}|BASH_SOURCE|\{BASH_SOURCE(?:\[[0-9@*]+\])?(?::?-[^}]*)?\})"?$'
)
PYTHON_INTERPRETER = re.compile(r"^(?:\S*/)?python(?:3(?:\.\d+)?)?$|^\$\{?(?:python|PYTHON)\w*\}?$")
GITHUB_EXPRESSION = re.compile(r"\$\{\{.*?\}\}", re.S)
WORKFLOW_SHELL_LANGUAGES = {
    "bash": "shell",
    "sh": "shell",
    "pwsh": "powershell",
    "powershell": "powershell",
    "python": "python",
    "node": "js",
    "cmd": "cmd",
}
CMD_COMMENT = re.compile(r"(?:^|&)[ \t@]*((?:::|[Rr][Ee][Mm](?=[ \t]|$)).*)")
JS_SCRIPT_TYPES = ("", "text/javascript", "application/javascript", "module")
HTML_TAG_OPEN = re.compile(r"<([A-Za-z][A-Za-z0-9:-]*)")
HTML_TEXT_ELEMENTS = ("script", "style", "textarea", "title")

COMMENT = "comment"
SELF_READ = "reads its own source"
UNREADABLE = "cannot be read"
NOT_UTF8 = "is not UTF-8 text; save it as UTF-8"
NUL_IN_PYTHON = "contains a NUL byte, which Python source cannot hold; remove the byte"

GUIDANCE = """\
ARC code carries no comments. They go stale, nothing enforces them, and they
give a place to justify a workaround instead of fixing it. Instead:
  - make the code say it: a clearer name, a type, or a smaller function;
  - if the comment states a rule, turn the rule into a test or a check;
  - put the reason for a change in the commit message or the pull request.
Allowed: tool directives (a shebang on line 1, lint, type and format pragmas),
license lines, `///` help text on clap command types, PowerShell comment-based
help (<# .SYNOPSIS ... #>) as the first thing in a .ps1 or .psm1 file, and a
Python module docstring that the module reads through __doc__."""
SELF_READ_GUIDANCE = """\
These scripts print their own source with sed, head, tail, awk, grep, cat or cut
on "$0" or BASH_SOURCE. That printed the header comments; with comments banned
it prints code. Put the text in a heredoc instead:
  usage() {
    cat <<'EOF'
  Usage: ...
  EOF
  }"""
UNREADABLE_GUIDANCE = """\
The checker could not read these files, so it did not check them. Each line
names the fix. Python is parsed by the interpreter that runs the checker
(Python %d.%d here)."""

Finding = collections.namedtuple("Finding", "path line column rule detail")
YamlEntry = collections.namedtuple("YamlEntry", "line indent column key value item")
YamlBlock = collections.namedtuple("YamlBlock", "parent entry lines")


class UnreadableSource(Exception):
    def __init__(self, reason, offset=0):
        super().__init__(reason)
        self.reason = reason
        self.offset = offset


def is_python_type_comment(comment):
    match = PYTHON_TYPE_COMMENT.match(comment)
    if match is None:
        return False
    annotation = match.group("annotation").strip()
    for mode in ("eval", "func_type"):
        try:
            tree = ast.parse(annotation, mode=mode)
        except (SyntaxError, ValueError):
            continue
        return all(isinstance(node, TYPE_EXPRESSION_NODES) for node in ast.walk(tree))
    return False


DIRECTIVE_CHECKS = {"python": (is_python_type_comment,)}


def line_end(text, start):
    end = text.find("\n", start)
    return len(text) if end < 0 else end


def skip_escaped(text, start, quote):
    n = len(text)
    i = start
    while i < n:
        c = text[i]
        if c == "\\":
            i += 2
            continue
        if c == quote:
            return i + 1
        i += 1
    return n


def skip_blanks(text, start):
    while start < len(text) and text[start] in " \t":
        start += 1
    return start


def is_word_char(c):
    return c.isalnum() or c == "_" or ord(c) > 127


def unquoted(word):
    if len(word) >= 2 and word[0] == word[-1] and word[0] in "\"'":
        return word[1:-1]
    return word


def rust_char_or_lifetime(text, start):
    n = len(text)
    if start + 1 < n and text[start + 1] == "\\":
        i = start + 2
        if i < n and text[i] == "u" and i + 1 < n and text[i + 1] == "{":
            close = text.find("}", i)
            i = n if close < 0 else close + 1
        elif i < n and text[i] == "x":
            i += 3
        else:
            i += 1
        if i < n and text[i] == "'":
            return i + 1
        return i
    if start + 2 < n and text[start + 2] == "'":
        return start + 3
    i = start + 1
    while i < n and is_word_char(text[i]):
        i += 1
    return i


def rust_tokens(text):
    tokens = []
    n = len(text)
    i = 0
    while i < n:
        c = text[i]
        if c.isspace():
            i += 1
            continue
        if text.startswith("//", i):
            end = line_end(text, i)
            doc = (text.startswith("///", i) and not text.startswith("////", i)) or text.startswith(
                "//!", i
            )
            tokens.append(("doc" if doc else "line", i, end))
            i = end
            continue
        if text.startswith("/*", i):
            depth = 0
            j = i
            while j < n:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    j += 1
            doc = (
                text.startswith("/**", i)
                and not text.startswith("/***", i)
                and not text.startswith("/**/", i)
            ) or text.startswith("/*!", i)
            tokens.append(("doc" if doc else "block", i, j))
            i = j
            continue
        if c == '"':
            end = skip_escaped(text, i + 1, '"')
            tokens.append(("literal", i, end))
            i = end
            continue
        if c == "'":
            end = rust_char_or_lifetime(text, i)
            tokens.append(("literal", i, end))
            i = end
            continue
        if c.isalpha() or c == "_" or ord(c) > 127:
            j = i + 1
            while j < n and is_word_char(text[j]):
                j += 1
            word = text[i:j]
            if j < n and word in ("r", "br", "cr") and text[j] in '#"':
                k = j
                while k < n and text[k] == "#":
                    k += 1
                if k < n and text[k] == '"':
                    closing = '"' + "#" * (k - j)
                    end = text.find(closing, k + 1)
                    end = n if end < 0 else end + len(closing)
                    tokens.append(("literal", i, end))
                    i = end
                    continue
            if j < n and word in ("b", "c") and text[j] == '"':
                end = skip_escaped(text, j + 1, '"')
                tokens.append(("literal", i, end))
                i = end
                continue
            if j < n and word == "b" and text[j] == "'":
                end = rust_char_or_lifetime(text, j)
                tokens.append(("literal", i, end))
                i = end
                continue
            tokens.append(("ident", i, j))
            i = j
            continue
        if c.isdigit():
            j = i + 1
            while j < n and (text[j].isalnum() or text[j] == "_"):
                j += 1
            tokens.append(("literal", i, j))
            i = j
            continue
        tokens.append(("punct", i, i + 1))
        i += 1
    return tokens


def matching_token(code, text, start, opener, closer):
    depth = 0
    for k in range(start, len(code)):
        word = text[code[k][1] : code[k][2]]
        if word == opener:
            depth += 1
        elif word == closer:
            depth -= 1
            if depth == 0:
                return k
    return len(code) - 1


def is_outer_rust_doc(text, start):
    return text.startswith("///", start) or text.startswith("/**", start)


def rust_imports_clap(text, code):
    words = [text[start:end] for _, start, end in code]
    for index, word in enumerate(words):
        if word == "use":
            following = index + 1
            while following < len(words) and words[following] == ":":
                following += 1
            if following < len(words) and words[following] == "clap":
                return True
        if word == "extern" and words[index + 1 : index + 3] == ["crate", "clap"]:
            return True
    return False


def derives_clap(attribute, imports_clap):
    if not re.search(r"\bderive\s*\(", attribute):
        return False
    return bool(CLAP_PATH_DERIVE.search(attribute)) or (
        imports_clap and bool(BARE_CLAP_DERIVE.search(attribute))
    )


def rust_clap_help_starts(text, tokens):
    allowed = set()
    code = [t for t in tokens if t[0] not in ("line", "block")]
    imports_clap = rust_imports_clap(text, code)

    def word(k):
        return text[code[k][1] : code[k][2]]

    docs = []
    clap = False
    i = 0
    while i < len(code):
        kind = code[i][0]
        if kind == "doc":
            if is_outer_rust_doc(text, code[i][1]):
                docs.append(code[i][1])
            else:
                docs = []
            i += 1
            continue
        if word(i) == "#" and i + 1 < len(code) and word(i + 1) in ("[", "!"):
            j = i + 1
            if word(j) == "!":
                j += 1
            close = matching_token(code, text, j, "[", "]")
            attribute = text[code[i][1] : code[close][2]]
            if derives_clap(attribute, imports_clap):
                clap = True
            i = close + 1
            continue
        if word(i) == "pub":
            i += 1
            if i < len(code) and word(i) == "(":
                i = matching_token(code, text, i, "(", ")") + 1
            continue
        if word(i) in ("struct", "enum") and clap:
            allowed.update(docs)
            j = i + 1
            while j < len(code) and word(j) not in ("{", ";"):
                if word(j) == "(":
                    j = matching_token(code, text, j, "(", ")")
                j += 1
            if j < len(code) and word(j) == "{":
                close = matching_token(code, text, j, "{", "}")
                for k in range(j, close):
                    if code[k][0] == "doc" and is_outer_rust_doc(text, code[k][1]):
                        allowed.add(code[k][1])
                i = close + 1
            else:
                i = j + 1
            docs = []
            clap = False
            continue
        docs = []
        clap = False
        i += 1
    return allowed


def rust_comments(text):
    tokens = rust_tokens(text)
    help_starts = rust_clap_help_starts(text, tokens)
    found = []
    for kind, start, end in tokens:
        if kind in ("line", "block", "doc"):
            found.append((kind, start, end, start in help_starts))
    return found


class JsLexer:
    def __init__(self, text, jsx):
        self.text = text
        self.n = len(text)
        self.jsx = jsx
        self.found = []

    def run(self):
        i = 0
        if self.text.startswith("#!"):
            end = line_end(self.text, 0)
            self.found.append(("shebang", 0, end))
            i = end
        self.code(i, None)
        return self.found

    def comment_at(self, i):
        text = self.text
        if text.startswith("//", i):
            end = line_end(text, i)
            self.found.append(("line", i, end))
            return end
        if text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = self.n if end < 0 else end + 2
            doc = text.startswith("/**", i) and not text.startswith("/**/", i)
            self.found.append(("doc" if doc else "block", i, end))
            return end
        return None

    def previous_significant(self, i):
        k = i - 1
        while k >= 0 and self.text[k].isspace():
            k -= 1
        return k

    def follows_dot(self, i):
        k = self.previous_significant(i)
        return k >= 0 and self.text[k] == "."

    def previous_word(self, i):
        end = self.previous_significant(i) + 1
        start = end
        while start > 0 and is_word_char(self.text[start - 1]):
            start -= 1
        if self.follows_dot(start):
            return ""
        return self.text[start:end]

    def code(self, i, closer):
        text, n = self.text, self.n
        regex_allowed = True
        depth = 0
        control_parens = []
        while i < n:
            c = text[i]
            if c.isspace() or c == "\ufeff":
                i += 1
                continue
            if c == "/":
                after = self.comment_at(i)
                if after is not None:
                    i = after
                    continue
                if regex_allowed:
                    end = self.regex(i + 1)
                    if end is not None:
                        i = end
                        regex_allowed = False
                        continue
                i += 1
                regex_allowed = True
                continue
            if c in "\"'":
                i = skip_escaped(text, i + 1, c)
                regex_allowed = False
                continue
            if c == "`":
                i = self.template(i + 1)
                regex_allowed = False
                continue
            if c == "{":
                depth += 1
                i += 1
                regex_allowed = True
                continue
            if c == "}":
                if depth == 0 and closer == "}":
                    return i + 1
                depth -= 1
                i += 1
                regex_allowed = True
                continue
            if c == "(":
                control_parens.append(self.previous_word(i) in JS_CONTROL_KEYWORDS)
                i += 1
                regex_allowed = True
                continue
            if c == ")":
                regex_allowed = control_parens.pop() if control_parens else False
                i += 1
                continue
            if c == "]":
                i += 1
                regex_allowed = False
                continue
            if c == "<" and self.jsx and regex_allowed and self.jsx_starts(i):
                i = self.jsx_element(i)
                regex_allowed = False
                continue
            if c.isalpha() or c in "_$#" or ord(c) > 127:
                j = i + 1
                while j < n and (is_word_char(text[j]) or text[j] == "$"):
                    j += 1
                regex_allowed = text[i:j] in JS_REGEX_KEYWORDS and not self.follows_dot(i)
                i = j
                continue
            if c.isdigit() or (c == "." and i + 1 < n and text[i + 1].isdigit()):
                j = i + 1
                while j < n and (text[j].isalnum() or text[j] in "_."):
                    j += 1
                i = j
                regex_allowed = False
                continue
            if text.startswith("++", i) or text.startswith("--", i):
                i += 2
                continue
            i += 1
            regex_allowed = True
        return n

    def template(self, i):
        text, n = self.text, self.n
        while i < n:
            c = text[i]
            if c == "\\":
                i += 2
                continue
            if c == "`":
                return i + 1
            if c == "$" and i + 1 < n and text[i + 1] == "{":
                i = self.code(i + 2, "}")
                continue
            i += 1
        return n

    def regex(self, i):
        text, n = self.text, self.n
        in_class = False
        while i < n:
            c = text[i]
            if c == "\n":
                return None
            if c == "\\":
                i += 2
                continue
            if in_class:
                if c == "]":
                    in_class = False
            elif c == "[":
                in_class = True
            elif c == "/":
                i += 1
                while i < n and is_word_char(text[i]):
                    i += 1
                return i
            i += 1
        return None

    def jsx_starts(self, i):
        text, n = self.text, self.n
        j = i + 1
        if j < n and text[j] == ">":
            return True
        if j >= n or not (text[j].isalpha() or text[j] in "_$"):
            return False
        k = j
        while k < n and (is_word_char(text[k]) or text[k] in "$.:-"):
            k += 1
        m = k
        while m < n and text[m].isspace():
            m += 1
        if m < n and text[m] in ",=":
            return False
        if text.startswith("extends", m) and (m + 7 >= n or not is_word_char(text[m + 7])):
            return False
        return True

    def jsx_element(self, i):
        text, n = self.text, self.n
        i += 1
        if i < n and text[i] == ">":
            return self.jsx_children(i + 1)
        while i < n and (is_word_char(text[i]) or text[i] in "$.:-"):
            i += 1
        while i < n:
            c = text[i]
            if c.isspace():
                i += 1
                continue
            if c == "/" and i + 1 < n and text[i + 1] in "/*":
                i = self.comment_at(i)
                continue
            if text.startswith("/>", i):
                return i + 2
            if c == ">":
                return self.jsx_children(i + 1)
            if c == "{":
                i = self.code(i + 1, "}")
                continue
            if c in "\"'":
                end = text.find(c, i + 1)
                i = n if end < 0 else end + 1
                continue
            if c == "<":
                depth = 0
                while i < n:
                    if text[i] == "<":
                        depth += 1
                    elif text[i] == ">":
                        depth -= 1
                        if depth == 0:
                            i += 1
                            break
                    i += 1
                continue
            i += 1
        return n

    def jsx_children(self, i):
        text, n = self.text, self.n
        while i < n:
            c = text[i]
            if text.startswith("</", i):
                end = text.find(">", i)
                return n if end < 0 else end + 1
            if c == "<":
                i = self.jsx_element(i)
                continue
            if c == "{":
                i = self.code(i + 1, "}")
                continue
            i += 1
        return n


def js_comments(text, jsx):
    return [(kind, start, end, False) for kind, start, end in JsLexer(text, jsx).run()]


def line_offsets(text):
    offsets = [0]
    for index, char in enumerate(text):
        if char == "\n":
            offsets.append(index + 1)
    return offsets


def python_docstring_nodes(tree):
    nodes = []
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            body = node.body
            if (
                body
                and isinstance(body[0], ast.Expr)
                and isinstance(body[0].value, ast.Constant)
                and isinstance(body[0].value.value, str)
            ):
                nodes.append((node, body[0]))
    return nodes


def python_reads_doc(tree):
    return any(isinstance(node, ast.Name) and node.id == "__doc__" for node in ast.walk(tree))


def char_column(lines, lineno, byte_column):
    line = lines[lineno - 1]
    return len(line.encode("utf-8")[:byte_column].decode("utf-8", "ignore"))


def python_parse_failure(error):
    message = error.msg if isinstance(error, SyntaxError) else str(error.args[0])
    return (
        "Python %d.%d cannot parse it (%s); if it uses newer syntax, run the checker with a "
        "newer Python, otherwise fix the syntax error" % (sys.version_info[0], sys.version_info[1], message)
    )


def python_failure_offset(error, offsets):
    if isinstance(error, SyntaxError):
        line = error.lineno or 1
    elif isinstance(error, tokenize.TokenError) and len(error.args) > 1:
        line = error.args[1][0]
    else:
        line = 1
    return offsets[min(max(line, 1), len(offsets)) - 1]


def is_encoding_declaration(lines, token):
    row, column = token.start
    if row > 2 or lines[row - 1][:column].strip():
        return False
    if row == 2 and lines[0].strip(" \t\f") and not lines[0].lstrip(" \t\f").startswith("#"):
        return False
    match = ENCODING_DECLARATION.match(token.string)
    if match is None:
        return False
    try:
        codecs.lookup(match.group("emacs") or match.group("vim") or match.group("plain"))
    except LookupError:
        return False
    return True


def python_comments(text):
    if "\x00" in text:
        raise UnreadableSource(NUL_IN_PYTHON, text.index("\x00"))
    offsets = line_offsets(text)
    try:
        tree = ast.parse(text)
        tokens = list(tokenize.generate_tokens(io.StringIO(text).readline))
    except (SyntaxError, ValueError, tokenize.TokenError) as error:
        raise UnreadableSource(python_parse_failure(error), python_failure_offset(error, offsets))
    lines = text.split("\n")
    found = []
    for token in tokens:
        if token.type != tokenize.COMMENT:
            continue
        start = offsets[token.start[0] - 1] + token.start[1]
        end = offsets[token.end[0] - 1] + token.end[1]
        if start == 0 and token.string.startswith("#!"):
            found.append(("shebang", start, end, False))
        elif is_encoding_declaration(lines, token):
            found.append(("coding", start, end, True))
        else:
            found.append(("line", start, end, False))
    reads_doc = python_reads_doc(tree)
    for owner, statement in python_docstring_nodes(tree):
        start = offsets[statement.lineno - 1] + char_column(
            lines, statement.lineno, statement.col_offset
        )
        end = offsets[statement.end_lineno - 1] + char_column(
            lines, statement.end_lineno, statement.end_col_offset
        )
        keep = isinstance(owner, ast.Module) and reads_doc
        found.append(("docstring", start, end, keep))
    return found


def shell_arguments(words):
    arguments = []
    target_follows = False
    for word in words:
        if target_follows:
            target_follows = False
            continue
        if SHELL_REDIRECTION.match(word):
            target_follows = True
            continue
        arguments.append(word)
    return arguments


def python_reads_program_from_stdin(arguments):
    option_value_follows = False
    for argument in arguments:
        if option_value_follows:
            option_value_follows = False
            continue
        if argument == "-":
            return True
        if not argument.startswith("-") or argument == "--":
            return False
        if argument.startswith("--"):
            continue
        flags = argument[1:]
        for index, flag in enumerate(flags):
            if flag in "cm":
                return False
            if flag in "WX":
                option_value_follows = index == len(flags) - 1
                break
    return True


def heredoc_feeds_python_program(words):
    arguments = shell_arguments(words)
    for index, word in enumerate(arguments):
        if PYTHON_INTERPRETER.match(unquoted(word)):
            return python_reads_program_from_stdin([unquoted(a) for a in arguments[index + 1 :]])
    return False


def shell_command_name(words):
    index = 0
    while index < len(words):
        word = words[index]
        if SHELL_REDIRECTION.match(word):
            index += 2
            continue
        if word in SHELL_COMMAND_PREFIXES or SHELL_ASSIGNMENT.match(word):
            index += 1
            continue
        return index
    return None


def self_reading_span(text, spans):
    words = [text[start:end] for start, end in spans]
    name = shell_command_name(words)
    if name is None or os.path.basename(unquoted(words[name])) not in SELF_READING_TOOLS:
        return None
    for index in range(name + 1, len(words)):
        previous = words[index - 1]
        reads_file = SHELL_INPUT_REDIRECTION.match(previous) or not SHELL_REDIRECTION.match(previous)
        if reads_file and SELF_REFERENCE.match(words[index]):
            return spans[name][0], spans[index][1]
    return None


class ShellCommand:
    def __init__(self):
        self.words = []
        self.open_word = None

    def extend(self, position):
        if self.open_word is None:
            self.open_word = position

    def close(self, position):
        if self.open_word is not None:
            self.words.append((self.open_word, position))
            self.open_word = None

    def add_operator(self, start, end):
        self.close(start)
        self.words.append((start, end))

    def add_redirection(self, text, start, end):
        if self.open_word is not None and text[self.open_word : start].isdigit():
            start, self.open_word = self.open_word, None
        self.add_operator(start, end)


class ShellLexer:
    def __init__(self, text):
        self.text = text
        self.n = len(text)
        self.found = []
        self.heredocs = []
        self.embedded = []
        self.commands = []

    def run(self):
        i = 0
        if self.text.startswith("#!"):
            end = line_end(self.text, 0)
            self.found.append(("shebang", 0, end))
            i = end
        while i < self.n:
            i = self.scan(i, None)
        return self.found

    def finish(self, command, position):
        command.close(position)
        if command.words:
            self.commands.append(command.words)
        return ShellCommand()

    def after_newline(self, i):
        text, n = self.text, self.n
        while self.heredocs:
            delimiter, strip_tabs, quoted, feeds_python = self.heredocs.pop(0)
            body_start = i
            body_end = n
            while i < n:
                end = text.find("\n", i)
                line = text[i : n if end < 0 else end]
                if (line.lstrip("\t") if strip_tabs else line) == delimiter:
                    body_end = i
                    i = n if end < 0 else end + 1
                    break
                i = n if end < 0 else end + 1
            if quoted and not strip_tabs and feeds_python:
                self.embedded.append(("python", body_start, body_end))
        return i

    def opens_extglob(self, i):
        text = self.text
        if i == 0 or text[i - 1] not in "?*+@!":
            return False
        if text[i - 1] == "!":
            return i >= 2 and text[i - 2] not in SHELL_WORD_BREAKS
        return True

    def scan(self, i, closer):
        text, n = self.text, self.n
        word_start = True
        case_depth = 0
        command = ShellCommand()
        while i < n:
            c = text[i]
            if c == "\n":
                command = self.finish(command, i)
                i = self.after_newline(i + 1)
                word_start = True
                continue
            if c in " \t\r":
                command.close(i)
                i += 1
                word_start = True
                continue
            if closer is not None and c == closer and not (c == ")" and case_depth > 0):
                self.finish(command, i)
                return i + 1
            if c == "#" and word_start:
                end = line_end(text, i)
                self.found.append(("line", i, end))
                i = end
                continue
            if text.startswith("\\\n", i):
                i += 2
                continue
            if word_start and text.startswith("=~", i):
                regex_start = skip_blanks(text, i + 2)
                if regex_start > i + 2 and regex_start < n and text[regex_start] not in "#\n":
                    command.extend(i)
                    i = self.conditional_regex(regex_start)
                    word_start = False
                    continue
            if c == "\\":
                command.extend(i)
                i += 2
                word_start = False
                continue
            if c == "'":
                command.extend(i)
                end = text.find("'", i + 1)
                i = n if end < 0 else end + 1
                word_start = False
                continue
            if c == '"':
                command.extend(i)
                i = self.double_quoted(i + 1)
                word_start = False
                continue
            if c == "$":
                command.extend(i)
                if text.startswith("$'", i):
                    i = skip_escaped(text, i + 2, "'")
                else:
                    i = self.expansion(i)
                word_start = False
                continue
            if c == "`":
                command.extend(i)
                i = self.scan(i + 1, "`")
                word_start = False
                continue
            if c == "(":
                if self.opens_extglob(i):
                    command.extend(i)
                    i = self.extglob(i + 1)
                    word_start = False
                    continue
                command = self.finish(command, i)
                if text.startswith("((", i):
                    i = self.arithmetic(i + 2)
                else:
                    i = self.scan(i + 1, ")")
                word_start = True
                continue
            if c == ")":
                command = self.finish(command, i)
                i += 1
                word_start = True
                continue
            if text.startswith("<<<", i):
                command.add_operator(i, i + 3)
                i += 3
                word_start = True
                continue
            if text.startswith("<<", i):
                command.close(i)
                words = [text[start:end] for start, end in command.words]
                i = self.heredoc_operator(i + 2, heredoc_feeds_python_program(words))
                word_start = True
                continue
            if c in "<>" and i + 1 < n and text[i + 1] == "(":
                command.extend(i)
                i = self.scan(i + 2, ")")
                word_start = False
                continue
            if c in "<>" or text.startswith("&>", i):
                end = i + 1
                while end < n and text[end] in "<>&|":
                    end += 1
                command.add_redirection(text, i, end)
                i = end
                word_start = True
                continue
            if c in ";&|":
                command = self.finish(command, i)
                i += 1
                word_start = True
                continue
            command.extend(i)
            if word_start and c.isalpha():
                end = i
                while end < n and (text[end].isalnum() or text[end] == "_"):
                    end += 1
                if end >= n or text[end] in SHELL_WORD_BREAKS:
                    word = text[i:end]
                    if word == "case":
                        case_depth += 1
                    elif word == "esac" and case_depth > 0:
                        case_depth -= 1
                i = end
                word_start = False
                continue
            i += 1
            word_start = False
        self.finish(command, n)
        return n

    def expansion(self, i):
        text, n = self.text, self.n
        if text.startswith("$((", i):
            return self.arithmetic(i + 3)
        if text.startswith("$(", i):
            return self.scan(i + 2, ")")
        if text.startswith("${", i):
            return self.parameter(i + 2)
        i += 1
        if i < n and text[i] in "#?$!@*-0123456789":
            i += 1
        return i

    def conditional_regex(self, i):
        text, n = self.text, self.n
        depth = 0
        while i < n:
            c = text[i]
            if c == "\n" or (depth == 0 and c in " \t\r;&<>"):
                return i
            if c == "\\":
                i += 2
                continue
            if c == "'":
                end = text.find("'", i + 1)
                i = n if end < 0 else end + 1
                continue
            if c == '"':
                i = self.double_quoted(i + 1)
                continue
            if c == "$":
                i = self.expansion(i)
                continue
            if c == "(":
                depth += 1
            elif c == ")":
                if depth == 0:
                    return i
                depth -= 1
            i += 1
        return n

    def extglob(self, i):
        text, n = self.text, self.n
        depth = 1
        while i < n:
            c = text[i]
            if c == "\n":
                return i
            if c == "\\":
                i += 2
                continue
            if c == "'":
                end = text.find("'", i + 1)
                i = n if end < 0 else end + 1
                continue
            if c == '"':
                i = self.double_quoted(i + 1)
                continue
            if c == "$":
                i = self.expansion(i)
                continue
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        return n

    def double_quoted(self, i):
        text, n = self.text, self.n
        while i < n:
            c = text[i]
            if c == "\\":
                i += 2
                continue
            if c == '"':
                return i + 1
            if c == "`":
                i = self.scan(i + 1, "`")
                continue
            if c == "$":
                i = self.expansion(i)
                continue
            i += 1
        return n

    def parameter(self, i):
        text, n = self.text, self.n
        depth = 1
        while i < n:
            c = text[i]
            if c == "\\":
                i += 2
                continue
            if c == "'":
                end = text.find("'", i + 1)
                i = n if end < 0 else end + 1
                continue
            if c == '"':
                i = self.double_quoted(i + 1)
                continue
            if c == "$":
                i = self.expansion(i)
                continue
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        return n

    def arithmetic(self, i):
        text, n = self.text, self.n
        depth = 2
        while i < n:
            c = text[i]
            if text.startswith("$(", i) and not text.startswith("$((", i):
                i = self.scan(i + 2, ")")
                continue
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        return n

    def heredoc_operator(self, i, feeds_python):
        text, n = self.text, self.n
        quoted = False
        strip_tabs = False
        if i < n and text[i] == "-":
            strip_tabs = True
            i += 1
        while i < n and text[i] in " \t":
            i += 1
        parts = []
        while i < n and text[i] not in SHELL_WORD_BREAKS:
            c = text[i]
            if c in "'\"":
                end = text.find(c, i + 1)
                end = n if end < 0 else end
                parts.append(text[i + 1 : end])
                quoted = True
                i = end + 1
                continue
            if c == "\\":
                parts.append(text[i + 1 : i + 2])
                quoted = True
                i += 2
                continue
            parts.append(c)
            i += 1
        if parts:
            self.heredocs.append(("".join(parts), strip_tabs, quoted, feeds_python))
        return i


@functools.lru_cache(maxsize=1)
def lexed_shell(text):
    lexer = ShellLexer(text)
    lexer.run()
    return lexer


def shell_comments(text):
    lexer = lexed_shell(text)
    found = [(kind, start, end, False) for kind, start, end in lexer.found]
    for _, start, end in lexer.embedded:
        try:
            inner = python_comments(text[start:end])
        except UnreadableSource as error:
            raise UnreadableSource(error.reason, start + error.offset)
        for kind, s, e, _ in inner:
            if kind != "docstring":
                found.append(("embedded-python", start + s, start + e, False))
    return found


def shell_self_reads(text):
    lexer = lexed_shell(text)
    spans = [self_reading_span(text, words) for words in lexer.commands]
    return [span for span in spans if span is not None]


BLOCK_SCALAR = re.compile(r"(?:^|[\s:\-])[|>](?:[+-]?[1-9]?|[1-9][+-])$")
YAML_ENTRY = re.compile(
    r"^( *)((?:- +)*)"
    r"(?:(\"(?:[^\"\\]|\\.)*\"|'(?:[^']|'')*'|[^\s#'\"\[\]{},:][^#]*?) *:(?= |$))?"
)


def yaml_entry(index, content):
    match = YAML_ENTRY.match(content)
    if not (match.group(2) or match.group(3)):
        return None
    indent = len(match.group(1))
    key = unquoted(match.group(3)) if match.group(3) else None
    value = unquoted(content[match.end() :].strip())
    return YamlEntry(index, indent, indent + len(match.group(2)), key, value, bool(match.group(2)))


def yaml_comments(text):
    found = []
    offsets = line_offsets(text)
    lines = text.split("\n")
    entries = []
    blocks = []
    block = None
    double_quoted = False
    single_quoted = False
    for index, line in enumerate(lines):
        start = offsets[index]
        stripped = line.lstrip(" ")
        indent = len(line) - len(stripped)
        if block is not None:
            if stripped.strip() == "" or indent > block.parent:
                block.lines.append((start, line))
                continue
            blocks.append(block)
            block = None
        starts_in_quote = double_quoted or single_quoted
        j = 0
        scalar_start = True
        content_end = len(line)
        while j < len(line):
            c = line[j]
            if double_quoted:
                if c == "\\":
                    j += 2
                    continue
                if c == '"':
                    double_quoted = False
                    scalar_start = False
                j += 1
                continue
            if single_quoted:
                if c == "'":
                    if j + 1 < len(line) and line[j + 1] == "'":
                        j += 2
                        continue
                    single_quoted = False
                    scalar_start = False
                j += 1
                continue
            if c == "#" and (j == 0 or line[j - 1] in " \t"):
                found.append(("line", start + j, start + len(line), False))
                content_end = j
                break
            if c in " \t":
                j += 1
                continue
            if scalar_start and c == '"':
                double_quoted = True
                j += 1
                continue
            if scalar_start and c == "'":
                single_quoted = True
                j += 1
                continue
            if c in "-?:" and (j + 1 == len(line) or line[j + 1] in " \t"):
                scalar_start = True
                j += 1
                continue
            if c in "[{,":
                scalar_start = True
                j += 1
                continue
            scalar_start = False
            j += 1
        if not starts_in_quote:
            entry = yaml_entry(index, line[:content_end])
            if entry is not None:
                entries.append(entry)
        if double_quoted or single_quoted:
            continue
        content = line[:content_end].rstrip()
        if BLOCK_SCALAR.search(content):
            lead = re.match(r"^(\s*)((?:-\s+)*)", content)
            if re.match(r"^\s*(?:-\s+)*[|>]", content):
                parent = len(lead.group(1)) + max(len(lead.group(2)) - 2, 0)
            else:
                parent = len(lead.group(1)) + len(lead.group(2))
            header = len(entries) - 1 if entries and entries[-1].line == index else None
            block = YamlBlock(parent, header, [])
    if block is not None:
        blocks.append(block)
    for block in blocks:
        language = yaml_block_language(entries, block.entry)
        if language is not None:
            found.extend(yaml_embedded_script(block.lines, language))
    return found


def yaml_parent(entries, position):
    column = entries[position].column
    for index in range(position - 1, -1, -1):
        if entries[index].column < column:
            return index
    return None


def yaml_siblings(entries, position):
    column = entries[position].column
    siblings = [position]
    if not entries[position].item:
        index = position - 1
        while index >= 0 and entries[index].column >= column:
            if entries[index].column == column:
                siblings.append(index)
                if entries[index].item:
                    break
            index -= 1
    index = position + 1
    while index < len(entries) and entries[index].column >= column:
        if entries[index].column == column:
            if entries[index].item:
                break
            siblings.append(index)
        index += 1
    return siblings


def yaml_child(entries, position, key):
    column = -1 if position is None else entries[position].column
    child_column = None
    for index in range(0 if position is None else position + 1, len(entries)):
        entry = entries[index]
        if entry.column <= column:
            return None
        if child_column is None:
            child_column = entry.column
        if entry.column == child_column and entry.key == key:
            return index
    return None


def yaml_value(entries, position, keys):
    for key in keys:
        position = yaml_child(entries, position, key)
        if position is None:
            return None
    return entries[position].value


def yaml_job(entries, position):
    ancestor = yaml_parent(entries, position)
    while ancestor is not None:
        parent = yaml_parent(entries, ancestor)
        if parent is not None and entries[parent].key == "jobs" and yaml_parent(entries, parent) is None:
            return ancestor
        ancestor = parent
    return None


def yaml_step_shell(entries, position):
    for sibling in yaml_siblings(entries, position):
        if entries[sibling].key == "shell" and entries[sibling].value:
            return entries[sibling].value
    job = yaml_job(entries, position)
    job_shell = None if job is None else yaml_value(entries, job, ("defaults", "run", "shell"))
    return job_shell or yaml_value(entries, None, ("defaults", "run", "shell")) or "bash"


def workflow_shell_language(shell):
    words = shell.split()
    program = re.sub(r"[\d.]+$", "", os.path.basename(words[0])) if words else "bash"
    return WORKFLOW_SHELL_LANGUAGES.get(program, "shell")


def yaml_is_github_script(entries, position):
    parent = yaml_parent(entries, position)
    if parent is None or entries[parent].key != "with":
        return False
    return any(
        entries[sibling].key == "uses" and re.match(r"actions/github-script(@|$)", entries[sibling].value)
        for sibling in yaml_siblings(entries, parent)
    )


def yaml_block_language(entries, position):
    if position is None:
        return None
    if entries[position].key == "run":
        return workflow_shell_language(yaml_step_shell(entries, position))
    if entries[position].key == "script" and yaml_is_github_script(entries, position):
        return "js"
    return None


def same_length_placeholder(match):
    return re.sub(r"[^\n]", "_", match.group(0))


def yaml_embedded_script(block_lines, language):
    indents = [len(line) - len(line.lstrip(" ")) for _, line in block_lines if line.strip()]
    if not indents:
        return []
    indent = min(indents)
    script = "\n".join(line[indent:] if line.strip() else "" for _, line in block_lines)
    script = GITHUB_EXPRESSION.sub(same_length_placeholder, script)
    script_offsets = line_offsets(script)

    def origin(offset):
        row = bisect.bisect_right(script_offsets, offset) - 1
        return block_lines[row][0] + indent + (offset - script_offsets[row])

    try:
        spans = find_comments(language, script)
    except UnreadableSource as error:
        raise UnreadableSource(error.reason, origin(error.offset))
    found = []
    for kind, start, end, _ in spans:
        if kind == "docstring":
            continue
        own_kind = kind if kind.startswith("embedded-") else "embedded-" + language
        found.append((own_kind, origin(start), origin(end), False))
    return found


def toml_comments(text):
    found = []
    n = len(text)
    i = 0
    while i < n:
        if text.startswith('"""', i):
            j = i + 3
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text.startswith('"""', j):
                    j += 3
                    while j < n and text[j] == '"':
                        j += 1
                    break
                j += 1
            i = j
            continue
        if text.startswith("'''", i):
            end = text.find("'''", i + 3)
            j = n if end < 0 else end + 3
            while j < n and text[j] == "'":
                j += 1
            i = j
            continue
        c = text[i]
        if c == '"':
            i = skip_escaped(text, i + 1, '"')
            continue
        if c == "'":
            end = text.find("'", i + 1)
            i = n if end < 0 else end + 1
            continue
        if c == "#":
            end = line_end(text, i)
            found.append(("line", i, end, False))
            i = end
            continue
        i += 1
    return found


def starts_c_char_literal(text, i):
    start = i
    while start > 0 and is_word_char(text[start - 1]):
        start -= 1
    return text[start:i] in ("", "L", "u", "U", "u8")


def c_char_literal_end(text, i):
    j = i + 1
    while j < len(text) and text[j] != "\n":
        if text[j] == "\\":
            j += 2
            continue
        if text[j] == "'":
            return j + 1
        j += 1
    return i + 1


def c_family_comments(text, nested, string_quotes, char_literals):
    found = []
    n = len(text)
    i = 0
    while i < n:
        if text.startswith("//", i):
            end = line_end(text, i)
            doc = text.startswith("///", i) and not text.startswith("////", i)
            found.append(("doc" if doc else "line", i, end, False))
            i = end
            continue
        if text.startswith("/*", i):
            if nested:
                depth = 0
                j = i
                while j < n:
                    if text.startswith("/*", j):
                        depth += 1
                        j += 2
                    elif text.startswith("*/", j):
                        depth -= 1
                        j += 2
                        if depth == 0:
                            break
                    else:
                        j += 1
                end = j
            else:
                end = text.find("*/", i + 2)
                end = n if end < 0 else end + 2
            doc = text.startswith("/**", i) and not text.startswith("/**/", i)
            found.append(("doc" if doc else "block", i, end, False))
            i = end
            continue
        c = text[i]
        if c in string_quotes:
            i = skip_escaped(text, i + 1, c)
            continue
        if char_literals and c == "'" and starts_c_char_literal(text, i):
            i = c_char_literal_end(text, i)
            continue
        i += 1
    return found


def css_comments(text):
    found = []
    n = len(text)
    i = 0
    while i < n:
        if text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            found.append(("block", i, end, False))
            i = end
            continue
        c = text[i]
        if c in "\"'":
            i = skip_escaped(text, i + 1, c)
            continue
        if text.startswith("url(", i):
            j = i + 4
            while j < n and text[j] in " \t":
                j += 1
            if j < n and text[j] not in "\"'":
                end = text.find(")", j)
                i = n if end < 0 else end + 1
                continue
        i += 1
    return found


def html_tag_end(text, i):
    n = len(text)
    value_may_start = False
    while i < n:
        c = text[i]
        if c == ">":
            return i + 1
        if c in "\"'" and value_may_start:
            end = text.find(c, i + 1)
            i = n if end < 0 else end + 1
            value_may_start = False
            continue
        if c == "=":
            value_may_start = True
        elif not c.isspace():
            value_may_start = False
        i += 1
    return n


def html_element_comments(name, attributes, body):
    if name == "style":
        return css_comments(body)
    if name != "script":
        return []
    kind = re.search(r"\btype\s*=\s*[\"']?([^\"'\s>]*)", attributes, re.IGNORECASE)
    script_type = kind.group(1).lower() if kind else ""
    return js_comments(body, False) if script_type in JS_SCRIPT_TYPES else []


def html_comments(text):
    found = []
    n = len(text)
    i = 0
    while i < n:
        if text[i] != "<":
            i += 1
            continue
        if text.startswith("<!--", i):
            end = text.find("-->", i + 4)
            end = n if end < 0 else end + 3
            found.append(("block", i, end, False))
            i = end
            continue
        tag = HTML_TAG_OPEN.match(text, i)
        if tag is None:
            i += 1
            continue
        name = tag.group(1).lower()
        body_start = html_tag_end(text, tag.end())
        if name not in HTML_TEXT_ELEMENTS:
            i = body_start
            continue
        closing = re.compile(r"</%s\s*>" % name, re.IGNORECASE).search(text, body_start)
        body_end = closing.start() if closing else n
        attributes = text[tag.end() : body_start - 1]
        for kind, start, end, keep in html_element_comments(name, attributes, text[body_start:body_end]):
            found.append((kind, start + body_start, end + body_start, keep))
        i = closing.end() if closing else n
    return found


def xml_comments(text):
    found = []
    i = text.find("<!--")
    while i >= 0:
        end = text.find("-->", i + 4)
        end = len(text) if end < 0 else end + 3
        found.append(("block", i, end, False))
        i = text.find("<!--", end)
    return found


def make_comment_column(line):
    depth = 0
    j = 0
    while j < len(line):
        c = line[j]
        if c == "\\" or line.startswith("$$", j):
            j += 2
            continue
        if c == "$" and j + 1 < len(line) and line[j + 1] in "({":
            depth += 1
            j += 2
            continue
        if depth and c in "({":
            depth += 1
        elif depth and c in ")}":
            depth -= 1
        elif c == "#" and depth == 0:
            return j
        j += 1
    return None


def make_comments(text):
    found = []
    offsets = line_offsets(text)
    for index, line in enumerate(text.split("\n")):
        start = offsets[index]
        if line.startswith("\t"):
            for _, s, e in ShellLexer(line[1:]).run():
                found.append(("line", start + 1 + s, start + 1 + e, False))
            continue
        column = make_comment_column(line)
        if column is not None:
            found.append(("line", start + column, start + len(line), False))
    return found


class PowerShellLexer:
    def __init__(self, text):
        self.text = text
        self.n = len(text)
        self.found = []

    def run(self):
        start = 0
        if self.text.startswith("#!"):
            start = line_end(self.text, 0)
            self.found.append(("shebang", 0, start, False))
        else:
            help_block = self.comment_based_help()
            if help_block is not None:
                self.found.append(("help", help_block[0], help_block[1], True))
                start = help_block[1]
        self.code(start, None)
        return self.found

    def comment_based_help(self):
        text = self.text
        start = re.match(r"[\ufeff\s]*", text).end()
        if not text.startswith("<#", start):
            return None
        close = text.find("#>", start + 2)
        if close < 0:
            return None
        body = [line.strip() for line in text[start + 2 : close].split("\n") if line.strip()]
        if not body or body[0].upper() not in POWERSHELL_HELP_KEYWORDS:
            return None
        return start, close + 2

    def code(self, i, closer):
        text, n = self.text, self.n
        in_bare_word = False
        depth = 0
        while i < n:
            c = text[i]
            if c.isspace() or c == "\ufeff":
                in_bare_word = False
                i += 1
                continue
            if text.startswith("<#", i):
                end = text.find("#>", i + 2)
                end = n if end < 0 else end + 2
                self.found.append(("block", i, end, False))
                in_bare_word = False
                i = end
                continue
            if c == "#" and not in_bare_word:
                end = line_end(text, i)
                self.found.append(("line", i, end, False))
                i = end
                continue
            if c == "`":
                in_bare_word = True
                i += 2
                continue
            if c == "@" and self.opens_here_string(i):
                i = self.here_string(i)
                in_bare_word = False
                continue
            if c == "'":
                i = self.single_quoted(i + 1)
                in_bare_word = False
                continue
            if c == '"':
                i = self.double_quoted(i + 1)
                in_bare_word = False
                continue
            if c == "$":
                i = self.variable(i + 1)
                in_bare_word = False
                continue
            if c == "(":
                depth += 1
            elif c == ")":
                if depth == 0 and closer == ")":
                    return i + 1
                depth -= 1
            in_bare_word = c not in POWERSHELL_OPERATOR_CHARS
            i += 1
        return n

    def opens_here_string(self, i):
        text = self.text
        if text[i + 1 : i + 2] not in ("'", '"'):
            return False
        return text[i + 2 : line_end(text, i)].strip() == ""

    def here_string(self, i):
        closing = "\n" + self.text[i + 1] + "@"
        end = self.text.find(closing, i + 2)
        return self.n if end < 0 else end + len(closing)

    def single_quoted(self, i):
        text, n = self.text, self.n
        while i < n:
            if text[i] == "'":
                if text.startswith("''", i):
                    i += 2
                    continue
                return i + 1
            i += 1
        return n

    def double_quoted(self, i):
        text, n = self.text, self.n
        while i < n:
            c = text[i]
            if c == "`":
                i += 2
                continue
            if c == '"':
                if text.startswith('""', i):
                    i += 2
                    continue
                return i + 1
            if text.startswith("$(", i):
                i = self.code(i + 2, ")")
                continue
            i += 1
        return n

    def variable(self, i):
        text, n = self.text, self.n
        if i < n and text[i] == "(":
            return self.code(i + 1, ")")
        if i < n and text[i] == "{":
            end = text.find("}", i)
            return n if end < 0 else end + 1
        if i < n and text[i] in "$?^":
            return i + 1
        while i < n and (is_word_char(text[i]) or text[i] == ":"):
            i += 1
        return i


def powershell_comments(text):
    return PowerShellLexer(text).run()


def cmd_comments(text):
    found = []
    offsets = line_offsets(text)
    for index, line in enumerate(text.split("\n")):
        match = CMD_COMMENT.search(line)
        if match:
            found.append(("line", offsets[index] + match.start(1), offsets[index] + len(line), False))
    return found


def nsis_comments(text):
    found = []
    n = len(text)
    i = 0
    at_word_start = True
    while i < n:
        if text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            found.append(("block", i, end, False))
            i = end
            continue
        c = text[i]
        if c in ";#" and at_word_start:
            end = line_end(text, i)
            found.append(("line", i, end, False))
            i = end
            continue
        if c in "\"'`":
            j = i + 1
            while j < n and text[j] != c and text[j] != "\n":
                j += 3 if text.startswith("$\\", j) else 1
            i = j + 1
            at_word_start = False
            continue
        at_word_start = c in " \t\r\n"
        i += 1
    return found


def leading_hash_line_comments(text, markers):
    found = []
    offsets = line_offsets(text)
    for index, line in enumerate(text.split("\n")):
        stripped = line.lstrip(" \t")
        if stripped[:1] in markers and stripped:
            start = offsets[index] + len(line) - len(stripped)
            found.append(("line", start, offsets[index] + len(line), False))
    return found


def hash_comments(text):
    found = []
    n = len(text)
    i = 0
    previous = "\n"
    while i < n:
        c = text[i]
        if c in "\"'":
            i = skip_escaped(text, i + 1, c)
            previous = c
            continue
        if c == "#" and previous in " \t\n;{}":
            end = line_end(text, i)
            found.append(("line", i, end, False))
            i = end
            continue
        previous = c
        i += 1
    return found


def find_comments(language, text):
    if language == "rust":
        return rust_comments(text)
    if language in ("ts", "js"):
        return js_comments(text, False)
    if language in ("tsx", "jsx"):
        return js_comments(text, True)
    if language == "python":
        return python_comments(text)
    if language == "shell":
        return shell_comments(text)
    if language == "yaml":
        return yaml_comments(text)
    if language == "toml":
        return toml_comments(text)
    if language == "css":
        return css_comments(text)
    if language == "html":
        return html_comments(text)
    if language == "solidity":
        return c_family_comments(text, False, "\"'", False)
    if language == "c":
        return c_family_comments(text, False, '"', True)
    if language == "wgsl":
        return c_family_comments(text, True, "", False)
    if language == "make":
        return make_comments(text)
    if language == "powershell":
        return powershell_comments(text)
    if language == "cmd":
        return cmd_comments(text)
    if language == "nsis":
        return nsis_comments(text)
    if language == "unit":
        return leading_hash_line_comments(text, "#;")
    if language == "hashline":
        return [c for c in leading_hash_line_comments(text, "#") if text[c[1] - 1 : c[1]] in ("", "\n")]
    if language == "hash":
        return hash_comments(text)
    if language == "xml":
        return xml_comments(text)
    return []


def directive_family(language, kind):
    if kind.startswith("embedded-"):
        return kind[len("embedded-") :]
    return DIRECTIVE_FAMILY.get(language, language)


def is_directive(language, kind, comment_text):
    family = directive_family(language, kind)
    patterns = DIRECTIVES.get(family, ()) + UNIVERSAL_DIRECTIVES
    if any(re.search(pattern, comment_text) for pattern in patterns):
        return True
    return any(check(comment_text) for check in DIRECTIVE_CHECKS.get(family, ()))


def is_allowed_comment(language, kind, start, comment_text):
    if kind == "shebang":
        return start == 0
    return is_directive(language, kind, comment_text)


def is_exempt(path):
    if path.startswith(THIRD_PARTY_PREFIXES + PROSE_PREFIXES + DATA_PREFIXES):
        return True
    rooted = "/" + path
    if any(part in rooted for part in THIRD_PARTY_PATH_PARTS):
        return True
    if any(part in rooted for part in DATA_PATH_PARTS) and language_of_name(path) is None:
        return True
    if path in DATA_FILES or path in SHA256_PINNED_FILES:
        return True
    return path.endswith(GENERATED_SUFFIXES)


def language_of_name(path):
    name = os.path.basename(path)
    if name in NAME_LANGUAGES:
        return NAME_LANGUAGES[name]
    if name.endswith(".plist.template"):
        return "xml"
    return EXTENSION_LANGUAGES.get(os.path.splitext(name)[1])


def language_of(path, text):
    language = language_of_name(path)
    if language is not None or os.path.splitext(os.path.basename(path))[1]:
        return language
    if text.startswith("#!"):
        first = text.split("\n", 1)[0]
        if re.search(r"\b(ba|z|da)?sh\b", first):
            return "shell"
        if "python" in first:
            return "python"
        if "node" in first:
            return "js"
    return None


def disallowed_comments(path, text):
    language = language_of(path, text)
    if language is None:
        return []
    result = []
    for kind, start, end, keep in find_comments(language, text):
        if keep or is_allowed_comment(language, kind, start, text[start:end]):
            continue
        result.append((kind, start, end))
    return sorted(result, key=operator.itemgetter(1))


def self_reading_commands(path, text):
    if language_of(path, text) != "shell":
        return []
    return shell_self_reads(text)


def git_paths(root, *options):
    output = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"] + list(options), capture_output=True, check=True
    ).stdout
    return [path for path in output.decode("utf-8").split("\0") if path]


def tracked_files(root):
    return git_paths(root)


def untracked_files(root):
    return git_paths(root, "--others", "--exclude-standard")


def position(text, offset):
    line = text.count("\n", 0, offset) + 1
    column = offset - (text.rfind("\n", 0, offset) + 1) + 1
    return line, column


def snippet(text, start, end):
    first_line = text[start:end].split("\n", 1)[0].strip()
    return first_line if len(first_line) <= 72 else first_line[:69] + "..."


def file_findings(path, text):
    try:
        spans = [(COMMENT, start, end) for _, start, end in disallowed_comments(path, text)]
        spans += [(SELF_READ, start, end) for start, end in self_reading_commands(path, text)]
    except UnreadableSource as error:
        line, column = position(text, error.offset)
        return [Finding(path, line, column, UNREADABLE, error.reason)]
    findings = []
    for rule, start, end in sorted(spans, key=operator.itemgetter(1)):
        line, column = position(text, start)
        findings.append(Finding(path, line, column, rule, snippet(text, start, end)))
    return findings


def scan(root, paths):
    findings = []
    for path in paths:
        if is_exempt(path):
            continue
        full = os.path.join(root, path)
        if os.path.islink(full) or not os.path.isfile(full):
            continue
        try:
            with open(full, encoding="utf-8") as handle:
                text = handle.read()
        except UnicodeDecodeError:
            if language_of_name(path) is not None:
                findings.append(Finding(path, 1, 1, UNREADABLE, NOT_UTF8))
            continue
        findings.extend(file_findings(path, text))
    return findings


def repository_path(root, path):
    if not os.path.isabs(path):
        return os.path.normpath(path).replace(os.sep, "/")
    resolved = os.path.join(os.path.realpath(os.path.dirname(path)), os.path.basename(path))
    relative = os.path.relpath(resolved, root)
    if relative == os.pardir or relative.startswith(os.pardir + os.sep):
        return path
    return relative.replace(os.sep, "/")


def report(findings):
    for finding in findings:
        print("%s:%d:%d: %s: %s" % finding)
    summaries = (
        (COMMENT, "comment(s) found", GUIDANCE),
        (SELF_READ, "script line(s) print their own source", SELF_READ_GUIDANCE),
        (UNREADABLE, "file(s) could not be read", UNREADABLE_GUIDANCE % sys.version_info[:2]),
    )
    for rule, summary, guidance in summaries:
        count = sum(1 for finding in findings if finding.rule == rule)
        if count:
            print("", file=sys.stderr)
            print("error: %d %s." % (count, summary), file=sys.stderr)
            print(guidance, file=sys.stderr)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="no_comments.py", description="Fail when ARC source in the repository carries comments."
    )
    parser.add_argument("paths", nargs="*", help="files to check, relative to the repository root")
    parser.add_argument("--root", default=None, help="repository root")
    args = parser.parse_args(argv)
    root = os.path.realpath(
        args.root or os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    )
    if args.paths:
        paths = [repository_path(root, path) for path in args.paths]
        missing = [path for path in paths if not os.path.isfile(os.path.join(root, path))]
        for path in missing:
            print(
                "no_comments.py: no such file: %s (pass paths relative to the repository root %s)"
                % (path, root),
                file=sys.stderr,
            )
        if missing:
            return 2
    else:
        paths = tracked_files(root) + untracked_files(root)
    findings = scan(root, paths)
    report(findings)
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
