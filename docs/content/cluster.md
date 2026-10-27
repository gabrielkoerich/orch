+++
title = "Cluster"
description = "Run orch on several machines over a tailnet, with leader failover"
weight = 12
+++

## What it is

Orch can run on several machines as one cluster. One machine is the leader and makes every decision: routing, recovery, review, merge, jobs and the GitHub poll. The machines agree on the task state through Raft ([openraft](https://github.com/databendlabs/openraft) 0.9), so when the leader goes down another machine takes over with no manual step.

The cluster is off by default. Without a `cluster` section in `~/.orch/config.yml`, orch runs on one machine exactly as before: no Raft, no listener, no network traffic.

The design and its trade-offs are in `docs/specs/2026-10-26-multi-machine-cluster-design.md`.

## Not built yet

- Assigning tasks to machines. In a cluster today only the leader dispatches agents. A learner or a follower tends its own sessions and nothing else. Keep the machine that runs your agents as a voter that can lead, and do not make it a learner yet.
- Sending GitHub writes from a non-leader to the leader.
- Recovery and shutdown limited to the tasks a machine owns.

## Requirements

- [Tailscale](https://tailscale.com) on every machine, with the `tailscale` CLI on `PATH`. Nodes find each other through `tailscale status`, and the cluster listens only on the machine's Tailscale address.
- The same orch version on every machine.
- On a machine that can lead: the GitHub token, the agent CLIs and the checkouts of your projects, as on a single machine today.

## Roles

| Role | Votes | Can lead | Runs agents | Needs |
|---|---|---|---|---|
| Voter | yes | yes | yes | always on, token, agents, checkouts |
| Witness | yes | no | no | always on, a few MB of disk. A small VPS or a home server works |
| Learner | no | no | yes, once assignment lands | nothing extra, it can sleep |

A decision needs a majority of voters, so voters must be machines that stay on. A machine that sleeps, like a laptop, should be a learner.

## How the leader is chosen

The voters elect the leader. You do not set it, and openraft 0.9 cannot hand leadership over on request. You choose who can lead through the roles:

- Only a voter without `witness: true` can become leader.
- A witness votes but never starts an election, so it never leads.
- A learner never votes and never leads.

So to keep the leader on two always-on machines, make them the two voters that can lead and add a witness as the third vote. The leader stays leader until it stops or loses its connection to the majority. Then the other voter takes over within about two seconds.

## Sizes

| Machines | Setup | Failover |
|---|---|---|
| 1 | no `cluster` section | none, as before |
| 2 | one voter and one learner | none. The learner adds capacity once assignment lands |
| 2 + witness | two voters and a witness | either voter can fail |
| 3 or more always on | three voters, the rest learners | one voter can fail. Five voters survive two |

Two machines alone cannot fail over safely. When the link between them drops, neither can tell a crash from a network split, so either both keep deciding or both stop. No consensus protocol avoids this: they all need a majority of votes. The witness is the cheapest third vote. Any always-on machine on the tailnet can host it.

## Configure

Generate one secret and use the same value on every machine:

```sh
openssl rand -hex 32
```

Find each machine's node name. It is the first label of its MagicDNS name, in lowercase, as `tailscale status` shows it. A machine named "m1 max" in the admin console is `m1-max`:

```sh
tailscale status --json | jq -r '.Self.DNSName' | cut -d. -f1
```

Add the same section to `~/.orch/config.yml` on every machine. Only `witness` differs between machines. Orch reads the secret as written and does not expand environment variables, so the file now holds a secret: keep it readable by your user only (`chmod 600 ~/.orch/config.yml`).

```yaml
cluster:
  secret: "<the value from openssl>"
  port: 7420
  voters: [m1-max, legion, vpsbr-16153223]
  witness: false   # true on the witness only
```

| Key | Default | Meaning |
|---|---|---|
| `cluster.secret` | none | Turns the cluster on. Every machine must have the same value |
| `cluster.port` | `7420` | Port the cluster listens on, on the Tailscale address only |
| `cluster.voters` | none | Node names of the voters, the same list on every machine. A machine not in the list is a learner |
| `cluster.witness` | `false` | This machine votes but never leads and runs no agents. It must be in `voters` |

You list no IP addresses. Each machine reads its own address and the voters' addresses from `tailscale status`, including voters that are offline.

## Allow the port on the tailnet

The secret already keeps other machines out, and an ACL also stops them from reaching the port at all. In the tailnet policy, allow `cluster.port` between the cluster machines only:

```json
{
  "hosts": {
    "m1-max": "100.73.204.46",
    "legion": "100.90.49.41",
    "vpsbr": "100.70.185.32",
    "m4": "100.123.138.124"
  },
  "acls": [
    {
      "action": "accept",
      "src": ["m1-max", "legion", "vpsbr", "m4"],
      "dst": ["m1-max:7420", "legion:7420", "vpsbr:7420", "m4:7420"]
    }
  ]
}
```

Use your own Tailscale IPs, from `tailscale status`. If your policy has no other rules, keep a rule that allows the rest of your traffic.

## Start it

1. Restart the voters first, the witness included: `orch service restart` on each. Each voter that can lead initializes the cluster with the full voter list, and they elect a leader once a majority is up.
2. Restart the learners. Every 30 seconds a learner looks for a member on the tailnet and asks to join, until one adds it.
3. Check the result on any machine:

```sh
orch nodes
```

```
NODE                 ADDRESS                ROLE     STATE
m1-max               100.73.204.46:7420     voter    leader
legion               100.90.49.41:7420      voter    this machine
vpsbr-16153223       100.70.185.32:7420     voter
```

A witness shows as a voter. `orch nodes` prints the members of the cluster as this machine's service sees them, so the service must be running.

## When a machine goes down

| Failure | Effect |
|---|---|
| A learner stops | Nothing else changes |
| One voter stops | The others keep the majority. If it was the leader, another voter that can lead takes over |
| The voters are split by the network | Only the side with a majority decides |
| No majority | No new decisions. Running agents continue, and decisions resume when a majority is back |

## Security

- Every request between machines carries the secret, and the listener binds only to the Tailscale address.
- When a machine looks for members, it proves it knows the secret with an HMAC and checks the same proof in the answer, so it never sends the secret to a machine that has not proven it.
- The CLI reaches its local service on `127.0.0.1` with a token from `~/.orch/state/cluster.port`, a file only your user can read.

## Files

| Path | Holds |
|---|---|
| `~/.orch/raft.db` | The Raft log and vote |
| `~/.orch/orch.db` | The task state, as before. The replicated tables are kept in step by the log |
| `~/.orch/state/cluster.port` | Port, token and process id of the local endpoint |

## Turn it off

Remove the `cluster` section and run `orch service restart`. The machine goes back to writing straight to its own database. `~/.orch/raft.db` stays on disk and is not read.
