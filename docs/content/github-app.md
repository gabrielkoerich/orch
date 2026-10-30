+++
title = "GitHub App"
description = "Real PR reviews from a bot account, and GitHub events through a relay instead of polling"
weight = 10
+++

A GitHub App is optional. Without one, orch reviews with an `## Automated Review` comment and polls GitHub every 45 seconds. With one:

- **Reviews** are real `APPROVE` or `REQUEST_CHANGES` reviews from the App's bot account, with each issue on a changed line as an inline comment, and an `orch/review` check run. See [Review Agent](@/review-agent.md).
- **Events** reach orch as soon as GitHub sends them, through a relay, so orch syncs at once instead of on the next poll.

The App only reviews. Every other call (pushes, pull requests, comments, labels, reads) uses your `gh` token, and you stay the author of the pull requests. GitHub refuses a review from a pull request's author, so the App must never open the pull requests it reviews.

## Create the App

On GitHub: **Settings → Developer settings → GitHub Apps → New GitHub App**, or the same under an organization.

| Field | Value |
|---|---|
| Name | Any, for example `the-orch`. The bot account is `<slug>[bot]` |
| Homepage URL | Any |
| Webhook | Off for reviews only. For events, the relay URL and secret, see [Events](#events) |
| Where can it be installed | Only on this account, unless organizations need it |

Repository permissions:

| Permission | Access | Why |
|---|---|---|
| Pull requests | Read and write | Submit the review and its inline comments |
| Checks | Read and write | Post the `orch/review` check run |
| Metadata | Read | Required by GitHub |
| Issues | Read | Only for events: `issues` and `issue_comment` |
| Contents | Read | Only for events: `push` |
| Actions | Read | Only for events: `workflow_run` |

Then:

1. **Generate a private key** on the App page. GitHub downloads a `.pem` file.
2. **Install the App** on the repositories orch works on: the App page → **Install App**. Your account and each organization are separate installations, and orch finds the right one per repository.
3. **Upload a logo**, if you like. GitHub shows it as a circle, so keep the drawing centered.

## Configure orch

Store the key in passbox. passbox keeps only the first line of a value, so put the PEM on one line with a literal `\n` for each newline. orch turns them back into newlines:

```bash
awk 'NF {printf "%s\\n", $0}' your-app.private-key.pem | passbox add orch/github-app-pem
```

Then in `~/.orch/config.yml`:

```yaml
gh:
  app:
    id: "123456"                           # the App ID, a number, from the App page
    private_key: passbox:orch/github-app-pem

review:
  mode: app                                # or in one project's .orch.yml
```

`private_key_path` (a path to the `.pem` file) and `private_key_env` (a variable holding the PEM) work instead of `private_key`. See [Configuration](@/configuration.md#github-app).

orch reads the key once after it starts, in the background, and keeps it in memory. A passbox read waits on Touch ID, so approve the prompt after a restart. Until the key loads, reviews fall back to the comment. The log confirms it:

```
GitHub App loaded, App reviews are on app_id=123456
```

`GitHub App unavailable` on the same line names the reason, for example a key that does not match the App.

### Commits as the bot

To show agent commits as the bot, with its logo, use the bot account's no-reply address. GitHub links a commit to an App only through this address, and it needs the bot account's user ID, not the App ID:

```bash
gh api "users/the-orch%5Bbot%5D" --jq .id    # use your App's slug
```

```yaml
git:
  name: the-orch[bot]
  email: <bot user id>+the-orch[bot]@users.noreply.github.com
```

You still open the pull requests, so the App can review them.

## Events

GitHub sends App webhooks to a URL, and orch has no public endpoint. A relay receives them instead: a small service, for example a Cloudflare Worker, that checks GitHub's signature, stores each event in order, and streams them to orch. orch connects out, so nothing listens on your machine.

1. Deploy the relay, with the App's webhook secret and a long random bearer token.
2. In the App: turn **Webhook** on, set the URL to the relay's endpoint and the secret, and subscribe to **Issues, Issue comment, Pull request, Pull request review, Check suite, Check run, Workflow run** and **Push**.
3. Give orch the relay URL and the token:

```yaml
relay:
  url: https://relay.example.com
  token: passbox:orch/relay-token
```

orch opens a websocket to `<url>/ws` with the token and keeps a cursor, so it resumes where it stopped after a restart. On each event:

| Event | orch |
|---|---|
| Any event | Starts a sync at once, for every project |
| `issues` `closed` | Also releases tasks waiting on that issue, since a merged orch PR closes its issue |
| A gap (the relay dropped events orch never read) | Syncs every project |

Polling stays on every 45 seconds as the fallback, so a relay that is down delays events but loses nothing. The log shows `relay stream connected` when the stream is up.
