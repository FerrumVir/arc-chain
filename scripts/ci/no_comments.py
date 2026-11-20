#!/usr/bin/env python3
import argparse
import ast
import io
import os
import re
import subprocess
import sys
import tokenize

THIRD_PARTY_PREFIXES = ("vendor/",)
PROSE_PREFIXES = ("docs/", "papers/")
DATA_PREFIXES = ("release/", "deploy/config/", "testnet/")
DATA_PATH_PARTS = ("/fixtures/", "/testdata/", "/node_modules/")
DATA_FILES = frozenset(
    (
        "genesis.toml",
        "testnet-seeds.txt",
        "desktop/src-tauri/resources/genesis.toml",
        "desktop/src-tauri/resources/testnet-seeds.txt",
    )
)
HASH_PINNED_FILES = frozenset(
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
    ".jsx": "tsx",
    ".js": "js",
    ".mjs": "js",
    ".cjs": "js",
    ".py": "python",
    ".sh": "shell",
    ".bash": "shell",
    ".yml": "yaml",
    ".yaml": "yaml",
    ".toml": "toml",
    ".css": "css",
    ".html": "html",
    ".sol": "solidity",
    ".metal": "c",
    ".cu": "c",
    ".wgsl": "wgsl",
    ".ps1": "powershell",
    ".nsh": "nsis",
    ".service": "unit",
    ".conf": "hash",
    ".plist": "xml",
}
NAME_LANGUAGES = {
    "Makefile": "make",
    ".gitignore": "hashline",
    ".dockerignore": "hashline",
    ".gitattributes": "hashline",
}

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
        r"@license\b",
        r"@preserve\b",
        r"^/\*!",
    ),
    "python": (
        r"^#!",
        r"^#.*\bcoding[:=]",
        r"^#\s*(type:|noqa\b|pragma\b|fmt:|pylint:|mypy:|pyright:|ruff:|isort:)",
        r"#\s*(noqa\b|type:\s*ignore|pragma:)",
    ),
    "shell": (r"^#!", r"^#\s*shellcheck\s"),
    "yaml": (
        r"^#\s*(yaml-language-server|yamllint|zizmor|actionlint)\b",
        r"^#cloud-(config|boothook)\b",
        r"^##\s*template:\s*jinja\b",
    ),
    "toml": (r"^#:schema\b",),
    "css": (r"^/\*!", r"stylelint-(disable|enable)", r"@license\b"),
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
    "powershell": (r"^#\s*requires\b", r"^#!"),
    "nsis": (),
    "unit": (),
    "hash": (),
    "hashline": (),
    "xml": (),
}
DIRECTIVE_FAMILY = {"ts": "js", "tsx": "js"}
UNIVERSAL_DIRECTIVES = (r"\bgitleaks:allow\b",)

CLAP_DERIVES = re.compile(r"\b(Parser|Args|Subcommand|ValueEnum)\b")
JS_REGEX_KEYWORDS = frozenset(
    "return typeof instanceof in of new delete void throw case do else yield await".split()
)
SHELL_WORD_BREAKS = " \t\r\n;&|()<>"
PYTHON_COMMAND = re.compile(
    r"(?:^|[\s\"'/;|&(])(?:python(?:3(?:\.\d+)?)?|\$\{?(?:python|PYTHON)\w*\}?)(?=[\s\"']|$)"
)

GUIDANCE = """\
ARC code carries no comments. They go stale, nothing enforces them, and they
give a place to justify a workaround instead of fixing it. Instead:
  - make the code say it: a clearer name, a type, or a smaller function;
  - if the comment states a rule, turn the rule into a test or a check;
  - put the reason for a change in the commit message or the pull request.
Allowed: tool directives (shebangs, lint, type and format pragmas), license
lines, `///` help text on clap command types, and a Python module docstring
that the module reads through __doc__."""


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


def is_word_char(c):
    return c.isalnum() or c == "_" or ord(c) > 127


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


def rust_clap_help_starts(text, tokens):
    allowed = set()
    code = [t for t in tokens if t[0] not in ("line", "block")]

    def word(k):
        return text[code[k][1] : code[k][2]]

    docs = []
    clap = False
    i = 0
    while i < len(code):
        kind = code[i][0]
        if kind == "doc":
            docs.append(code[i][1])
            i += 1
            continue
        if word(i) == "#" and i + 1 < len(code) and word(i + 1) in ("[", "!"):
            j = i + 1
            if word(j) == "!":
                j += 1
            close = matching_token(code, text, j, "[", "]")
            attribute = text[code[i][1] : code[close][2]]
            if re.search(r"\bderive\s*\(", attribute) and CLAP_DERIVES.search(attribute):
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
                    if code[k][0] == "doc":
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
        self.jsx_comment_containers = []

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

    def code(self, i, closer):
        text, n = self.text, self.n
        regex_allowed = True
        depth = 0
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
            if c in ")]":
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
                regex_allowed = text[i:j] in JS_REGEX_KEYWORDS
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
        if m < n and text[m] == ",":
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
                first = len(self.found)
                end = self.code(i + 1, "}")
                inner = self.found[first:]
                residue = []
                cursor = i + 1
                for _, start, stop in inner:
                    residue.append(text[cursor:start])
                    cursor = stop
                residue.append(text[cursor : end - 1])
                if inner and "".join(residue).strip() == "":
                    self.jsx_comment_containers.append((i, end))
                i = end
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


def python_comments(text):
    offsets = line_offsets(text)
    found = []
    for token in tokenize.generate_tokens(io.StringIO(text).readline):
        if token.type == tokenize.COMMENT:
            start = offsets[token.start[0] - 1] + token.start[1]
            end = offsets[token.end[0] - 1] + token.end[1]
            kind = "shebang" if start == 0 and token.string.startswith("#!") else "line"
            found.append((kind, start, end, False))
    tree = ast.parse(text)
    reads_doc = python_reads_doc(tree)
    lines = text.split("\n")
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


class ShellLexer:
    def __init__(self, text):
        self.text = text
        self.n = len(text)
        self.found = []
        self.heredocs = []
        self.embedded = []

    def run(self):
        i = 0
        if self.text.startswith("#!"):
            end = line_end(self.text, 0)
            self.found.append(("shebang", 0, end))
            i = end
        while i < self.n:
            i = self.scan(i, None)
        return self.found

    def after_newline(self, i):
        text, n = self.text, self.n
        while self.heredocs:
            delimiter, strip_tabs, quoted, command = self.heredocs.pop(0)
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
            if quoted and not strip_tabs and PYTHON_COMMAND.search(command):
                self.embedded.append(("python", body_start, body_end))
        return i

    def scan(self, i, closer):
        text, n = self.text, self.n
        word_start = True
        case_depth = 0
        while i < n:
            c = text[i]
            if c == "\n":
                i = self.after_newline(i + 1)
                word_start = True
                continue
            if c in " \t\r":
                i += 1
                word_start = True
                continue
            if closer is not None and c == closer and not (c == ")" and case_depth > 0):
                return i + 1
            if c == "#" and word_start:
                end = line_end(text, i)
                self.found.append(("line", i, end))
                i = end
                continue
            if c == "\\":
                i += 2
                word_start = False
                continue
            if c == "'":
                end = text.find("'", i + 1)
                i = n if end < 0 else end + 1
                word_start = False
                continue
            if c == '"':
                i = self.double_quoted(i + 1)
                word_start = False
                continue
            if c == "$":
                if text.startswith("$'", i):
                    i = skip_escaped(text, i + 2, "'")
                elif text.startswith("$((", i):
                    i = self.arithmetic(i + 3)
                elif text.startswith("$(", i):
                    i = self.scan(i + 2, ")")
                elif text.startswith("${", i):
                    i = self.parameter(i + 2)
                else:
                    i += 1
                    if i < n and text[i] in "#?$!@*-0123456789":
                        i += 1
                word_start = False
                continue
            if c == "`":
                i = self.scan(i + 1, "`")
                word_start = False
                continue
            if c == "(":
                if text.startswith("((", i):
                    i = self.arithmetic(i + 2)
                else:
                    i = self.scan(i + 1, ")")
                word_start = True
                continue
            if c == ")":
                i += 1
                word_start = True
                continue
            if text.startswith("<<<", i):
                i += 3
                word_start = True
                continue
            if text.startswith("<<", i):
                i = self.heredoc_operator(i + 2)
                word_start = True
                continue
            if c in "<>" and i + 1 < n and text[i + 1] == "(":
                i = self.scan(i + 2, ")")
                word_start = False
                continue
            if c in ";&|<>":
                i += 1
                word_start = True
                continue
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
                if text.startswith("$((", i):
                    i = self.arithmetic(i + 3)
                    continue
                if text.startswith("$(", i):
                    i = self.scan(i + 2, ")")
                    continue
                if text.startswith("${", i):
                    i = self.parameter(i + 2)
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
                if text.startswith("$((", i):
                    i = self.arithmetic(i + 3)
                    continue
                if text.startswith("$(", i):
                    i = self.scan(i + 2, ")")
                    continue
                if text.startswith("${", i):
                    i = self.parameter(i + 2)
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

    def heredoc_operator(self, i):
        text, n = self.text, self.n
        command_start = text.rfind("\n", 0, i) + 1
        while command_start >= 2 and text[command_start - 2] == "\\":
            command_start = text.rfind("\n", 0, command_start - 2) + 1
        command = text[command_start : i - 2]
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
            self.heredocs.append(("".join(parts), strip_tabs, quoted, command))
        return i


def shell_spans(text):
    lexer = ShellLexer(text)
    spans = list(lexer.run())
    for language, start, end in lexer.embedded:
        try:
            inner = python_comments(text[start:end])
        except (SyntaxError, ValueError, tokenize.TokenError):
            continue
        for kind, s, e, _ in inner:
            if kind != "docstring":
                spans.append(("embedded-python", start + s, start + e))
    return spans


def shell_comments(text):
    return [(kind, start, end, False) for kind, start, end in shell_spans(text)]


BLOCK_SCALAR = re.compile(r"(?:^|[\s:\-])[|>](?:[+-]?[1-9]?|[1-9][+-])$")
RUN_KEY = re.compile(r"^\s*(?:-\s+)*run\s*:")


def yaml_comments(text):
    found = []
    offsets = line_offsets(text)
    lines = text.split("\n")
    block = None
    double_quoted = False
    single_quoted = False

    def finish(block_state):
        if block_state is not None and block_state[1] and block_state[2]:
            shell = yaml_step_shell(lines, block_state[3], block_state[4], len(block_state[2]))
            found.extend(yaml_embedded_script(block_state[2], shell))

    for index, line in enumerate(lines):
        start = offsets[index]
        stripped = line.lstrip(" ")
        indent = len(line) - len(stripped)
        if block is not None:
            if stripped.strip() == "" or indent > block[0]:
                block[2].append((start, line))
                continue
            finish(block)
            block = None
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
        if double_quoted or single_quoted:
            continue
        content = line[:content_end].rstrip()
        if BLOCK_SCALAR.search(content):
            lead = re.match(r"^(\s*)((?:-\s+)*)", content)
            key_column = len(lead.group(1)) + len(lead.group(2))
            if re.match(r"^\s*(?:-\s+)*[|>]", content):
                parent = len(lead.group(1)) + max(len(lead.group(2)) - 2, 0)
            else:
                parent = key_column
            block = (parent, bool(RUN_KEY.match(content)), [], index, key_column)
    finish(block)
    return found


def yaml_key_column(line):
    lead = re.match(r"^(\s*)((?:-\s+)*)", line)
    return len(lead.group(1)), len(lead.group(1)) + len(lead.group(2)), bool(lead.group(2))


def yaml_step_shell(lines, header, column, block_length):
    siblings = [lines[header]]
    k = header - 1
    if not yaml_key_column(lines[header])[2]:
        while k >= 0:
            line = lines[k]
            k -= 1
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            indent, key, item = yaml_key_column(line)
            if indent > column and not item:
                continue
            if key == column:
                siblings.append(line)
                if item:
                    break
                continue
            break
    k = header + 1 + block_length
    while k < len(lines):
        line = lines[k]
        k += 1
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent, key, item = yaml_key_column(line)
        if indent > column and not item:
            continue
        if key == column and not item:
            siblings.append(line)
            continue
        break
    for line in siblings:
        match = re.match(r"^\s*(?:-\s+)*shell\s*:\s*([A-Za-z0-9_-]+)", line)
        if match:
            return match.group(1)
    return "bash"


def yaml_embedded_script(block_lines, shell):
    content = [(start, line) for start, line in block_lines]
    indents = [len(line) - len(line.lstrip(" ")) for _, line in content if line.strip()]
    if not indents:
        return []
    indent = min(indents)
    script_lines = [line[indent:] if line.strip() else "" for _, line in content]
    script = "\n".join(script_lines)
    script_offsets = line_offsets(script)
    if shell in ("pwsh", "powershell"):
        spans = [(kind, start, end) for kind, start, end, _ in powershell_comments(script)]
        kind_name = "embedded-powershell"
    elif shell == "python":
        spans = [(kind, start, end) for kind, start, end, _ in python_comments(script)]
        kind_name = "embedded-python"
    else:
        spans = shell_spans(script)
        kind_name = "embedded-shell"
    found = []
    for kind, start, end in spans:
        if kind == "docstring":
            continue
        own_kind = kind if kind.startswith("embedded-") else kind_name
        row = 0
        while row + 1 < len(script_offsets) and script_offsets[row + 1] <= start:
            row += 1
        column = start - script_offsets[row]
        origin = content[row][0] + indent + column
        found.append((own_kind, origin, origin + (end - start), False))
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


def c_family_comments(text, nested, strings):
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
        if c in strings:
            i = skip_escaped(text, i + 1, c)
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


SCRIPT_OR_STYLE = re.compile(r"<(script|style)\b([^>]*)>", re.IGNORECASE)
JS_SCRIPT_TYPES = ("", "text/javascript", "application/javascript", "module")


def html_comments(text):
    found = []
    n = len(text)
    i = 0
    while i < n:
        if text.startswith("<!--", i):
            end = text.find("-->", i + 4)
            end = n if end < 0 else end + 3
            found.append(("block", i, end, False))
            i = end
            continue
        match = SCRIPT_OR_STYLE.match(text, i)
        if match:
            tag = match.group(1).lower()
            closing = re.compile(r"</%s\s*>" % tag, re.IGNORECASE).search(text, match.end())
            body_end = closing.start() if closing else n
            body = text[match.end() : body_end]
            if tag == "style":
                inner = css_comments(body)
            else:
                kind = re.search(r"\btype\s*=\s*[\"']?([^\"'\s>]*)", match.group(2), re.IGNORECASE)
                script_type = kind.group(1).lower() if kind else ""
                inner = js_comments(body, False) if script_type in JS_SCRIPT_TYPES else []
            for comment_kind, start, end, keep in inner:
                found.append((comment_kind, start + match.end(), end + match.end(), keep))
            i = closing.end() if closing else n
            continue
        i += 1
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


def make_comments(text):
    found = []
    offsets = line_offsets(text)
    for index, line in enumerate(text.split("\n")):
        start = offsets[index]
        if line.startswith("\t"):
            for kind, s, e in ShellLexer(line[1:]).run():
                if kind == "line":
                    found.append(("line", start + 1 + s, start + 1 + e, False))
            continue
        j = 0
        while j < len(line):
            if line[j] == "\\":
                j += 2
                continue
            if line[j] == "#":
                found.append(("line", start + j, start + len(line), False))
                break
            j += 1
    return found


def powershell_comments(text):
    found = []
    n = len(text)
    i = 0
    while i < n:
        if text.startswith("<#", i):
            end = text.find("#>", i + 2)
            end = n if end < 0 else end + 2
            found.append(("block", i, end, False))
            i = end
            continue
        if text.startswith("@'", i) or text.startswith('@"', i):
            closing = "\n" + text[i + 1] + "@"
            end = text.find(closing, i + 2)
            i = n if end < 0 else end + len(closing)
            continue
        c = text[i]
        if c == "#":
            end = line_end(text, i)
            found.append(("line", i, end, False))
            i = end
            continue
        if c == "'":
            j = i + 1
            while j < n:
                if text[j] == "'":
                    if j + 1 < n and text[j + 1] == "'":
                        j += 2
                        continue
                    break
                j += 1
            i = j + 1
            continue
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "`" else 1
            i = j + 1
            continue
        i += 1
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
    if language == "tsx":
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
        return c_family_comments(text, False, "\"'")
    if language == "c":
        return c_family_comments(text, False, '"')
    if language == "wgsl":
        return c_family_comments(text, True, "")
    if language == "make":
        return make_comments(text)
    if language == "powershell":
        return powershell_comments(text)
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


def is_directive(language, kind, comment_text):
    if kind == "shebang":
        return True
    family = DIRECTIVE_FAMILY.get(language, language)
    if kind.startswith("embedded-"):
        family = kind[len("embedded-") :]
    patterns = DIRECTIVES.get(family, ()) + UNIVERSAL_DIRECTIVES
    return any(re.search(pattern, comment_text) for pattern in patterns)


def is_exempt(path):
    if path.startswith(THIRD_PARTY_PREFIXES + PROSE_PREFIXES + DATA_PREFIXES):
        return True
    if any(part in "/" + path for part in DATA_PATH_PARTS):
        return True
    if path in DATA_FILES or path in HASH_PINNED_FILES:
        return True
    return path.endswith(GENERATED_SUFFIXES)


def language_of(path, text):
    name = os.path.basename(path)
    if name in NAME_LANGUAGES:
        return NAME_LANGUAGES[name]
    if name.endswith(".plist.template"):
        return "xml"
    extension = os.path.splitext(name)[1]
    if extension in EXTENSION_LANGUAGES:
        return EXTENSION_LANGUAGES[extension]
    if not extension and text.startswith("#!"):
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
        if keep or is_directive(language, kind, text[start:end]):
            continue
        result.append((kind, start, end))
    return result


def tracked_files(root):
    output = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True
    ).stdout
    return [path for path in output.decode("utf-8").split("\0") if path]


def position(text, offset):
    line = text.count("\n", 0, offset) + 1
    column = offset - (text.rfind("\n", 0, offset) + 1) + 1
    return line, column


def scan(root, paths):
    violations = []
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
            continue
        if language_of(path, text) is None:
            continue
        try:
            found = disallowed_comments(path, text)
        except (SyntaxError, tokenize.TokenError) as error:
            violations.append((path, 1, 1, "cannot be read: %s" % error))
            continue
        for kind, start, end in found:
            line, column = position(text, start)
            snippet = text[start:end].split("\n", 1)[0].strip()
            if len(snippet) > 72:
                snippet = snippet[:69] + "..."
            violations.append((path, line, column, snippet))
    return violations


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="no_comments.py", description="Fail when tracked ARC source carries comments."
    )
    parser.add_argument("paths", nargs="*", help="repository-relative files to check")
    parser.add_argument("--root", default=None, help="repository root")
    args = parser.parse_args(argv)
    root = args.root or os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    paths = args.paths or tracked_files(root)
    violations = scan(root, paths)
    for path, line, column, snippet in violations:
        print("%s:%d:%d: comment: %s" % (path, line, column, snippet))
    if violations:
        print("", file=sys.stderr)
        print("error: %d comment(s) found." % len(violations), file=sys.stderr)
        print(GUIDANCE, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
