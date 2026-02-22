"""Lightweight cron expression matcher. No external dependencies.

Usage:
  python3 cron_match.py "0 9 * * *"
    Exit 0 if the expression matches the current minute, 1 otherwise.

  python3 cron_match.py "0 9 * * *" --since "2026-02-19T08:00:00Z"
    Exit 0 if ANY minute between --since and now matches the cron expr.
    Useful for catching up on missed jobs after downtime.
    Lookback capped at 24 hours.

Supports: *, ranges (1-5), steps (*/15, 1-5/2), lists (1,3,5),
and aliases (@hourly, @daily, @weekly, @monthly, @yearly).
"""
import calendar
import sys
import time

ALIASES = {
    "@yearly": "0 0 1 1 *",
    "@annually": "0 0 1 1 *",
    "@monthly": "0 0 1 * *",
    "@weekly": "0 0 * * 0",
    "@daily": "0 0 * * *",
    "@midnight": "0 0 * * *",
    "@hourly": "0 * * * *",
}


def match_field(field, value, min_val, max_val):
    for part in field.split(","):
        if "/" in part:
            base, step = part.split("/", 1)
            step = int(step)
            if base == "*":
                start, end = min_val, max_val
            elif "-" in base:
                start, end = (int(x) for x in base.split("-", 1))
            else:
                start, end = int(base), max_val
            if start <= value <= end and (value - start) % step == 0:
                return True
        elif "-" in part:
            start, end = (int(x) for x in part.split("-", 1))
            if start <= value <= end:
                return True
        elif part == "*":
            return True
        else:
            if int(part) == value:
                return True
    return False


def matches_cron(expr):
    expr = ALIASES.get(expr.strip(), expr.strip())
    fields = expr.split()
    if len(fields) != 5:
        return False

    now = time.localtime()
    minute, hour, dom, month, dow = fields

    # Python tm_wday: 0=Mon..6=Sun → cron dow: 0=Sun, 1=Mon..6=Sat
    cron_dow = (now.tm_wday + 1) % 7

    return (
        match_field(minute, now.tm_min, 0, 59)
        and match_field(hour, now.tm_hour, 0, 23)
        and match_field(dom, now.tm_mday, 1, 31)
        and match_field(month, now.tm_mon, 1, 12)
        and match_field(dow, cron_dow, 0, 6)
    )


def _parse_cron(expr):
    """Resolve aliases and split into 5 fields, or return None."""
    expr = ALIASES.get(expr.strip(), expr.strip())
    fields = expr.split()
    return fields if len(fields) == 5 else None


def _matches_time(fields, lt):
    """Check if a time.struct_time matches the 5 cron fields."""
    minute, hour, dom, month, dow = fields
    cron_dow = (lt.tm_wday + 1) % 7  # Python Mon=0 → cron Sun=0
    return (
        match_field(minute, lt.tm_min, 0, 59)
        and match_field(hour, lt.tm_hour, 0, 23)
        and match_field(dom, lt.tm_mday, 1, 31)
        and match_field(month, lt.tm_mon, 1, 12)
        and match_field(dow, cron_dow, 0, 6)
    )


def has_match_since(expr, since_iso):
    """Check if cron had any match between since_iso and now."""
    fields = _parse_cron(expr)
    if not fields:
        return False

    # Parse ISO timestamp (strip trailing Z or timezone)
    clean = since_iso[:19]
    since_ts = calendar.timegm(time.strptime(clean, "%Y-%m-%dT%H:%M:%S"))
    now_ts = time.time()

    # Cap at 24h to prevent runaway iteration
    max_lookback = 24 * 60 * 60
    if now_ts - since_ts > max_lookback:
        since_ts = now_ts - max_lookback

    # Check each minute from since+1min to now (exclusive)
    t = since_ts + 60
    while t < now_ts:
        lt = time.gmtime(t)
        if _matches_time(fields, lt):
            return True
        t += 60
    return False


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[2] == "--since":
        sys.exit(0 if has_match_since(sys.argv[1], sys.argv[3]) else 1)
    elif len(sys.argv) == 2:
        sys.exit(0 if matches_cron(sys.argv[1]) else 1)
    else:
        print("usage: cron_match.py 'CRON_EXPRESSION' [--since ISO_TIMESTAMP]", file=sys.stderr)
        sys.exit(2)
