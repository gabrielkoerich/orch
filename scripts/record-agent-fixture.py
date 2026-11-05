#!/usr/bin/env python3
"""Record an agent CLI run as a parser fixture under tests/fixtures/.

A maintainer runs this by hand. It never runs in CI or in tests, the output is committed as data.

  record  runs one agent with orch's command line on a throwaway git repo
  import  takes an archived orch attempt (~/.orch/state/<owner>/<repo>/tasks/<id>/attempts/<n>),
          for cases a CLI cannot produce on demand, such as a rate limit

Both strip paths, usernames, ids and the names given with --redact, write a `#` header with the
agent, CLI version, date, scenario and exit code, and refuse to write a file gitleaks flags.

  scripts/record-agent-fixture.py record claude success
  scripts/record-agent-fixture.py record codex killed --kill-after 20
  scripts/record-agent-fixture.py import claude rate_limit <attempt-dir> --keep rate_limit_event --keep '"type":"result"'
"""

import argparse
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
FIXTURES = REPO / "tests" / "fixtures"

SYSTEM_PROMPT = """You are an autonomous coding agent working in a git repository.
Your final output MUST be a single JSON object and nothing else, with these fields:
{"status": "done|needs_review|blocked", "summary": "...", "accomplished": [], "remaining": [], "files_changed": [], "blockers": [], "reason": "", "learnings": []}
"""

PROMPTS = {
    "success": "Create a file named hello.txt that contains the word hello, commit it with the message 'Add hello', then reply with the JSON object.",
    "killed": "Write a 2000 word essay about the history of version control into essay.md, one section at a time, then commit it and reply with the JSON object.",
    "background": "Start the shell command `sleep 45 && echo done > bg.txt` in the background with the Bash tool's run_in_background option. Do not wait for it. Reply with the JSON object right away, then stop.",
    "subagent": "Use the Task tool to start a subagent in the background that counts the lines of every file in this repository. Do not wait for it. Reply with the JSON object right away, then stop.",
}

# Mirrors build_command in src/engine/runner/agents/{claude,codex,opencode}.rs with the default
# autonomous permissions. Keep these in step with those functions
COMMANDS = {
    "claude": '{timeout} claude -p --verbose {model} --permission-mode bypassPermissions --output-format stream-json --append-system-prompt "{sys}" < "{msg}"',
    "codex": "{timeout} codex {model} exec -c 'sandbox_workspace_write.network_access=true' -c 'shell_environment_policy.inherit=all' --sandbox workspace-write -c 'approval_policy=\"never\"' --json - < <(cat \"{sys}\" \"{msg}\")",
    "opencode": "OPENCODE_CONFIG_CONTENT='{{\"snapshot\":false}}' {timeout} opencode run {model} --dangerously-skip-permissions --format json - < <(cat \"{sys}\" \"{msg}\")",
}

VERSION_ARGS = {"claude": ["claude", "--version"], "codex": ["codex", "--version"], "opencode": ["opencode", "--version"]}

# Claude lists the operator's hooks, MCP servers, plugins, skills and commands in these events
CLAUDE_PRIVATE_SUBTYPES = {"hook_started", "hook_response"}
CLAUDE_PRIVATE_INIT_FIELDS = ("mcp_servers", "plugins", "skills", "slash_commands", "agents", "plugin_errors", "memory_paths")


def cli_version(agent):
    try:
        out = subprocess.run(VERSION_ARGS[agent], capture_output=True, text=True, timeout=30)
        return out.stdout.strip().splitlines()[0]
    except (OSError, subprocess.SubprocessError, IndexError):
        return "unknown"


class Scrubber:
    """Replace machine paths, ids and names with stable placeholders"""

    def __init__(self, workdir, redact):
        self.ids = {}
        self.pairs = []
        for path in filter(None, [workdir, os.path.realpath(workdir) if workdir else None]):
            self.pairs.append((path, "/work"))
        home = str(Path.home())
        self.pairs += [(os.path.realpath(home), "/home/user"), (home, "/home/user")]
        self.pairs += [(word, "<repo>") for word in redact]
        user = os.environ.get("USER", "")
        self.user = re.compile(rf"\b{re.escape(user)}\b") if len(user) > 2 else None

    def _id(self, kind, value, fmt):
        key = (kind, value)
        if key not in self.ids:
            self.ids[key] = fmt(sum(1 for k in self.ids if k[0] == kind) + 1)
        return self.ids[key]

    def text(self, s):
        for old, new in sorted(self.pairs, key=lambda p: -len(p[0])):
            s = s.replace(old, new)
        s = re.sub(r"/(?:private/)?var/folders/[^\s\"'\\\\]+", "/tmp/x", s)
        s = re.sub(r"/(?:private/)?tmp/claude-\d+/[^\s\"'\\\\]+", "/tmp/claude/x", s)
        s = re.sub(
            r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
            lambda m: self._id("uuid", m.group(0), lambda n: f"00000000-0000-4000-8000-{n:012d}"),
            s,
        )
        s = re.sub(
            r"\b(ses|msg|prt|call|toolu|req|err)_(?=[A-Za-z]*[0-9])[A-Za-z0-9]{16,}(?![A-Za-z0-9])",
            lambda m: self._id(m.group(1), m.group(0), lambda n: f"{m.group(1)}_{n:024d}"),
            s,
        )
        if self.user:
            s = self.user.sub("user", s)
        return s

    def line(self, line):
        """Drop or trim the events that list the operator's setup, then scrub the text"""
        try:
            event = json.loads(line)
        except ValueError:
            return self.text(line)
        if isinstance(event, dict) and event.get("type") == "system":
            if event.get("subtype") in CLAUDE_PRIVATE_SUBTYPES:
                return None
            if event.get("subtype") == "init":
                for field in CLAUDE_PRIVATE_INIT_FIELDS:
                    if field in event:
                        event[field] = []
                event["tools"] = [t for t in event.get("tools", []) if not t.startswith("mcp__")]
                return self.text(json.dumps(event, separators=(",", ":"), ensure_ascii=False))
        return self.text(line)


def scrub(raw, scrubber, keep):
    # `script` echoes the EOF of an empty stdin as ^D and two backspaces
    raw = re.sub(r"^\^D\x08\x08", "", raw)
    lines = []
    for line in raw.replace("\r\n", "\n").split("\n"):
        if keep and not any(re.search(k, line) for k in keep):
            continue
        out = scrubber.line(line)
        if out is not None:
            lines.append(out)
    return "\n".join(lines).strip("\n") + "\n"


def leak_check(text):
    """Scan with gitleaks' default rules, the repo config allowlists tests/ so it is not used"""
    if not shutil.which("gitleaks"):
        sys.exit("gitleaks is not installed, refusing to write an unscanned fixture")
    with tempfile.TemporaryDirectory() as tmp:
        Path(tmp, "fixture.jsonl").write_text(text)
        res = subprocess.run(
            ["gitleaks", "detect", "--no-git", "--no-banner", "--redact", "--source", tmp, "--exit-code", "3"],
            capture_output=True,
            text=True,
        )
    if res.returncode == 3:
        sys.exit("gitleaks flagged the recording, nothing written:\n" + res.stdout + res.stderr)


def write_fixture(out, header, body, stderr):
    lines = [f"# {k}: {v}" for k, v in header.items()]
    lines += [f"# stderr: {line}" for line in stderr.strip("\n").split("\n") if line.strip()]
    text = "\n".join(lines) + "\n" + body
    leak_check(text)
    out.write_text(text)
    print(f"wrote {out} ({len(body.splitlines())} lines, exit {header['exit']})")
    for word in re.findall(r"/Users/\w+|/home/(?!user\b)\w+", text):
        print(f"warning: {word} is still in the fixture, check it before committing", file=sys.stderr)


def record(args):
    agent = args.agent
    prompt = args.prompt or PROMPTS.get(args.scenario)
    if not prompt:
        sys.exit(f"no built-in prompt for {args.scenario}, pass --prompt")
    work = Path(tempfile.mkdtemp(prefix="orch-fixture-"))
    repo = work / "repo"
    repo.mkdir()
    env = dict(os.environ, GIT_AUTHOR_NAME="fixture", GIT_COMMITTER_NAME="fixture",
               GIT_AUTHOR_EMAIL="fixture@example.com", GIT_COMMITTER_EMAIL="fixture@example.com")
    env.pop("CLAUDECODE", None)
    for pair in args.env:
        key, _, value = pair.partition("=")
        env[key] = value
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=repo, check=True)
    (repo / "README.md").write_text("# scratch\n")
    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "init"], cwd=repo, check=True, env=env)
    sys_file, msg_file = work / "prompt-sys.md", work / "prompt-msg.md"
    sys_file.write_text(SYSTEM_PROMPT)
    # Claude gets the system prompt's path, as orch passes it, so the output rules go in both files
    msg_file.write_text(prompt + "\n\n" + SYSTEM_PROMPT)
    model = f"--model '{args.model}'" if args.model else ""
    timeout = f"timeout -s TERM {args.kill_after}" if args.kill_after else f"timeout {args.timeout}"
    cmd = COMMANDS[agent].format(timeout=timeout, model=model, sys=sys_file, msg=msg_file)
    stdout_file, stderr_file = work / "stdout", work / "stderr"
    script = work / "agent_cmd.sh"
    script.write_text(f"#!/usr/bin/env bash\n{cmd} 2>'{stderr_file}'\n")
    # Same pty wrapper as the runner script, so the CLI sees a tty as it does under orch
    pty = ["script", "-q", "/dev/null", "/bin/bash", str(script)]
    if sys.platform.startswith("linux"):
        pty = ["script", "-qec", f"/bin/bash {script}", "/dev/null"]
    print(f"running {agent} in {repo}", file=sys.stderr)
    with open(stdout_file, "w") as out:
        proc = subprocess.run(pty, cwd=repo, env=env, stdout=out, stdin=subprocess.DEVNULL)
    scrubber = Scrubber(str(repo), args.redact + [str(work)])
    raw = stdout_file.read_text(errors="replace")
    stderr = stderr_file.read_text(errors="replace") if stderr_file.exists() else ""
    header = {
        "agent": agent,
        "cli": cli_version(agent),
        "date": datetime.date.today().isoformat(),
        "scenario": args.scenario,
        "source": "recorded with scripts/record-agent-fixture.py",
        "exit": proc.returncode,
    }
    out = Path(args.out) if args.out else FIXTURES / f"{agent}_{args.scenario}.jsonl"
    write_fixture(out, header, scrub(raw, scrubber, args.keep), scrubber.text(stderr))
    if not args.keep_workdir:
        shutil.rmtree(work, ignore_errors=True)


def import_attempt(args):
    attempt = Path(args.attempt).expanduser()
    raw = (attempt / "output.json").read_text(errors="replace")
    stderr_path = attempt / "stderr.txt"
    stderr = stderr_path.read_text(errors="replace") if stderr_path.exists() else ""
    exit_path = attempt / "exit.txt"
    exit_code = exit_path.read_text().strip() if exit_path.exists() else "unknown"
    workdir = None
    runner_path = attempt / "runner.sh"
    if runner_path.exists():
        m = re.search(r"^cd '([^']+)'", runner_path.read_text(), re.M)
        workdir = m.group(1) if m else None
    redact = list(args.redact)
    # The owner/repo pair is in the attempt path: <state>/<owner>/<repo>/tasks/<id>/attempts/<n>.
    # A bare repo name is often a common word, so it is redacted only when passed with --redact
    parts = attempt.resolve().parts
    if "tasks" in parts:
        i = parts.index("tasks")
        redact.append(f"{parts[i - 2]}/{parts[i - 1]}")
    scrubber = Scrubber(workdir, redact)
    date = datetime.date.fromtimestamp((attempt / "output.json").stat().st_mtime).isoformat()
    header = {
        "agent": args.agent,
        "cli": args.cli or "unknown, archived before versions were recorded",
        "date": date,
        "scenario": args.scenario,
        "source": "archived orch run" + (", trimmed to lines matching " + " | ".join(args.keep) if args.keep else ""),
        "exit": exit_code,
    }
    out = Path(args.out) if args.out else FIXTURES / f"{args.agent}_{args.scenario}.jsonl"
    write_fixture(out, header, scrub(raw, scrubber, args.keep), scrubber.text(stderr))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("record", "import"):
        s = sub.add_parser(name)
        s.add_argument("agent", choices=sorted(COMMANDS))
        s.add_argument("scenario")
        if name == "import":
            s.add_argument("attempt", help="an attempt dir under ~/.orch/state")
            s.add_argument("--cli", help="the CLI version the run used, when known")
        else:
            s.add_argument("--model")
            s.add_argument("--prompt", help="task prompt, default is the scenario's built-in one")
            s.add_argument("--timeout", type=int, default=600)
            s.add_argument("--kill-after", type=int, help="SIGTERM the agent after N seconds")
            s.add_argument("--env", action="append", default=[], help="KEY=VALUE for the agent")
            s.add_argument("--keep-workdir", action="store_true")
        s.add_argument("--keep", action="append", default=[], help="keep only lines matching this regex")
        s.add_argument("--redact", action="append", default=[], help="replace this text with <repo>")
        s.add_argument("--out", help="output path, default tests/fixtures/<agent>_<scenario>.jsonl")
    args = p.parse_args()
    (record if args.cmd == "record" else import_attempt)(args)


if __name__ == "__main__":
    main()
