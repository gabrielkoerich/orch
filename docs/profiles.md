# Named routing profiles

The `profile` router selects an installed CLI, model and optional reasoning effort
from a named entry. It uses the same dependency gate, task states, dispatch,
review, cooldown and failover as the other router modes.

For example, an operator can configure two roles using the same CLI:

```yaml
router:
  mode: profile
profiles:
  designer:
    agent: codex
    model: design-model
    effort: high
  implementer:
    agent: codex
    model: implementation-model
    effort: medium
```

Replace these illustrative model names with models available to the local CLI.
Label an issue `profile:designer` or `profile:implementer`. Profile names are
case-sensitive. `agent:claude`, `agent:codex`, etc. keep their existing executor
meaning; profile names never occupy that namespace. Other router modes ignore
`profile:` labels. Profile mode requires exactly one known profile label for a
new selection; invalid configuration, an empty model/effort, an unconfigured
agent, or a missing CLI produces a normal routing error. Profiles with no effort
can use any configured executor. Explicit effort is supported for Codex
(`model_reasoning_effort`), Claude (`--effort`) and OpenCode (`--variant`).
OpenCode models use its normal `provider/model` syntax; an explicit profile effort
takes precedence over a legacy `@variant` suffix. Model and effort support are
ultimately checked by the native CLI, as in ordinary routing.

Dependencies are checked before profile selection. To implement a design handoff,
use separate design and implementation issues and make the latter depend on the
former. Put role instructions in each issue body; a profile does not add a role
prompt or change the task lifecycle.

The selected model and effort are stored with the ordinary route and survive a
restart. They are routing preferences, not immutable assignments. A cooled
initial selection falls through to ordinary routing among available agents and
models. Profile mode uses round-robin for that fallback; when all choices are
cooled, the existing cooldown error retries on a later tick. Once a task is
routed, normal recovery may replace its agent or model;
stored recovery choices take precedence over the profile label. The old effort
is dropped when either changes, and ordinary rerouting clears the profile choice.
Reviews continue to use the ordinary independent review selection.

This feature selects local CLI arguments. It does not authenticate a persona,
bind a host/account/provider identity, create distributed leases, or attest that
a CLI supports a requested model. Those worker/cluster concerns are separate.
No environment policy or registry file is required, and profile mode does not
refuse daemon startup. Configuration remains operator-managed.
