#!/usr/bin/env python3
import json
import os
import sys


def strip_fences(text: str) -> str:
    if "```" not in text:
        return text
    parts = text.split("```")
    if len(parts) >= 3:
        body = parts[1]
        if body.startswith("json"):
            body = body[len("json") :]
        if body.startswith("\n"):
            body = body[1:]
        return body
    return text


def parse_payload(payload):
    if isinstance(payload, str):
        if "```" in payload:
            parts = payload.split("```")
            if len(parts) >= 3:
                payload = parts[1]
        text = strip_fences(payload).strip()
        if text.startswith("json"):
            text = text[len("json") :].lstrip()
        try:
            return json.loads(text)
        except Exception:
            return json.loads(json.dumps(text))
    return payload


def normalize(raw: str):
    # Try single JSON object first (claude --output-format json)
    try:
        obj = json.loads(raw)
        if isinstance(obj, dict) and "result" in obj:
            parsed = parse_payload(obj["result"])
        else:
            parsed = parse_payload(obj)
        if isinstance(parsed, dict):
            return json.dumps(parsed, separators=(",", ":"))
    except Exception:
        pass

    # Parse NDJSON lines (claude --output-format stream-json)
    lines = [l for l in raw.splitlines() if l.strip().startswith("{")]
    events = []
    for line in lines:
        try:
            events.append(json.loads(line))
        except Exception:
            continue

    # Look for final "result" event (claude stream-json format)
    for ev in reversed(events):
        if ev.get("type") == "result" and "result" in ev:
            parsed = parse_payload(ev["result"])
            if isinstance(parsed, dict):
                return json.dumps(parsed, separators=(",", ":"))

    # Look for text events with fenced JSON
    for ev in reversed(events):
        if ev.get("type") == "text":
            text = ev.get("part", {}).get("text", "")
            if text:
                parsed = parse_payload(text)
                if isinstance(parsed, dict):
                    return json.dumps(parsed, separators=(",", ":"))

    # Codex format — check agent_message texts
    for ev in reversed(events):
        if ev.get("type") == "item.completed":
            item = ev.get("item", {})
            if item.get("type") == "agent_message":
                text = item.get("text", "")
                if text:
                    parsed = parse_payload(text)
                    if isinstance(parsed, dict) and "status" in parsed:
                        return json.dumps(parsed, separators=(",", ":"))

    # Codex format — check command_execution output for JSON written via cat/echo
    for ev in reversed(events):
        if ev.get("type") == "item.completed":
            item = ev.get("item", {})
            if item.get("type") == "command_execution":
                cmd = item.get("command", "")
                if "output" in cmd and ".json" in cmd:
                    # Extract JSON from the heredoc in the command
                    import re
                    m = re.search(r'\{[\s\S]*"status"[\s\S]*\}', cmd)
                    if m:
                        try:
                            parsed = json.loads(m.group(0))
                            if isinstance(parsed, dict) and "status" in parsed:
                                return json.dumps(parsed, separators=(",", ":"))
                        except Exception:
                            pass

    # Codex format — fallback: any agent_message with parseable dict
    for ev in reversed(events):
        if ev.get("type") == "item.completed":
            item = ev.get("item", {})
            if item.get("type") == "agent_message":
                text = item.get("text", "")
                if text:
                    parsed = parse_payload(text)
                    if isinstance(parsed, dict):
                        return json.dumps(parsed, separators=(",", ":"))

    return None


def extract_tool_history(raw: str) -> list:
    """Extract tool calls from claude JSON events stream."""
    history = []
    for line in raw.splitlines():
        try:
            ev = json.loads(line.strip())
        except Exception:
            continue
        if ev.get("type") == "tool_use":
            tool = ev.get("tool", ev.get("name", ""))
            inp = ev.get("input", {})
            history.append({"tool": tool, "input": inp})
        elif ev.get("type") == "tool_result":
            if history:
                history[-1]["error"] = ev.get("is_error", False)
    return history


def summarize_tool_history(history: list) -> str:
    """Format tool history as human-readable summary."""
    lines = []
    for h in history[-20:]:
        tool = h.get("tool", "?")
        inp = h.get("input", {})
        err = " [ERROR]" if h.get("error") else ""
        if tool == "Bash":
            lines.append(f"  $ {inp.get('command', '?')}{err}")
        elif tool in ("Edit", "Write"):
            lines.append(f"  {tool}: {inp.get('file_path', '?')}{err}")
        elif tool == "Read":
            lines.append(f"  Read: {inp.get('file_path', '?')}{err}")
        elif tool == "Glob":
            lines.append(f"  Glob: {inp.get('pattern', '?')}{err}")
        elif tool == "Grep":
            lines.append(f"  Grep: {inp.get('pattern', '?')}{err}")
        else:
            lines.append(f"  {tool}{err}")
    return "\n".join(lines)


def extract_usage(raw: str) -> dict:
    """Extract token usage from claude JSON events stream."""
    usage = {"input_tokens": 0, "output_tokens": 0}
    for line in raw.splitlines():
        try:
            ev = json.loads(line.strip())
        except Exception:
            continue
        # claude --output-format json emits a final "result" event with usage
        if ev.get("type") == "result" and "usage" in ev:
            u = ev["usage"]
            usage["input_tokens"] = u.get("input_tokens", 0)
            usage["output_tokens"] = u.get("output_tokens", 0)
        # Also check for usage in top-level (codex format)
        elif "usage" in ev and isinstance(ev["usage"], dict):
            u = ev["usage"]
            if u.get("input_tokens", 0) > usage["input_tokens"]:
                usage["input_tokens"] = u.get("input_tokens", 0)
            if u.get("output_tokens", 0) > usage["output_tokens"]:
                usage["output_tokens"] = u.get("output_tokens", 0)
    return usage


def main():
    raw = os.environ.get("RAW_RESPONSE")
    if raw is None:
        raw = sys.stdin.read()

    if not raw:
        sys.exit(1)

    if "--tool-history" in sys.argv:
        history = extract_tool_history(raw)
        if history:
            print(json.dumps(history))
        sys.exit(0)

    if "--tool-summary" in sys.argv:
        history = extract_tool_history(raw)
        if history:
            print(summarize_tool_history(history))
        sys.exit(0)

    if "--usage" in sys.argv:
        usage = extract_usage(raw)
        print(json.dumps(usage))
        sys.exit(0)

    out = normalize(raw)
    if out is None:
        sys.exit(1)
    print(out)


if __name__ == "__main__":
    main()
