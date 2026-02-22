#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

TASK_ID="${1:-}"
if [ -z "$TASK_ID" ]; then
  log_err "Usage: stream_task.sh <task_id>"
  exit 1
fi

ensure_state_dir
STREAM_FILE="${STATE_DIR}/stream-${TASK_ID}.jsonl"

if [ ! -f "$STREAM_FILE" ]; then
  echo "No stream file for task $TASK_ID"
  exit 1
fi

tail -f "$STREAM_FILE" 2>/dev/null | python3 -c "
import sys, json
for line in sys.stdin:
    try:
        ev = json.loads(line.strip())
    except:
        continue
    t = ev.get('type', '')
    if t == 'assistant' and 'message' in ev:
        for block in ev['message'].get('content', []):
            if block.get('type') == 'text':
                print(block['text'], flush=True)
    elif t == 'tool_use':
        tool = ev.get('tool', ev.get('name', ''))
        inp = ev.get('input', {})
        if tool == 'Bash':
            print(f'  \$ {inp.get(\"command\", \"?\")[:120]}', flush=True)
        elif tool in ('Edit', 'Write'):
            print(f'  {tool}: {inp.get(\"file_path\", \"?\")}', flush=True)
        elif tool == 'Read':
            print(f'  Read: {inp.get(\"file_path\", \"?\")}', flush=True)
        else:
            print(f'  {tool}', flush=True)
    elif t == 'result':
        cost = ev.get('total_cost_usd', 0)
        dur = ev.get('duration_ms', 0) / 1000
        print(f'--- Done ({dur:.0f}s, \${cost:.2f}) ---', flush=True)
"
