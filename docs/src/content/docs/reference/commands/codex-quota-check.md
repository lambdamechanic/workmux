---
title: "codex-quota-check"
description: Inspect offline native Codex quota recovery prerequisites
---

Native quota recovery is **disabled**. This diagnostic reads local schemas and
optional saved native payloads; it cannot enable recovery, contact Codex, submit
input, or change session or authorization state. Terminal quota observations
from `status` are not recovery authorization.

```bash
codex app-server generate-json-schema --experimental --out /tmp/codex-schema
workmux codex-quota-check --schema-dir /tmp/codex-schema
```

Schema generation is offline. The checker requires the generated `v2` files for
`TurnStartParams`, `TurnSteerParams`, `ThreadQueueStartParams`,
`GetAccountRateLimitsResponse`, `TurnCompletedNotification`, and
`ThreadReadResponse`.

Optional inputs:

- `--turn-event FILE`: a saved full `turn/completed` notification envelope
  (`method` and `params`). Only failed turns with the exact native
  `codexErrorInfo: "usageLimitExceeded"` are classified `quota_failed`.
- `--thread-response FILE`: the saved `thread/read` result object containing
  `thread`, without its JSON-RPC envelope. Reports thread ID, session-tree ID,
  status, and active holds without verifying ownership or pause/cancel intent.
- `--rate-limits-response FILE`: the saved `account/rateLimits/read` result
  object, without its JSON-RPC envelope. Reports account and bucket identifiers;
  saved headroom or reset times do not establish freshness or authorization.

These files are unauthenticated observations, not a joined or validated live
session snapshot. Capacity errors, throughput limits, session budgets, quoted
messages, interrupted turns, and completed turns do not become quota failures.
Question and approval holds remain visible in the report. No continuation is
attempted, including on repeated invocations or after process restarts.

## Output and deployment status

The JSON always contains `recovery_enabled: false` and four typed blockers.
Exit zero means the files were inspected, **not** that recovery is available.
Missing or malformed required input fails before printing a report. Schema
fields are an inventory, not proof of behavioral compatibility; adding familiar
field names cannot enable this command. `reviewed_codex_version` and
`reviewed_source_revision` identify the review baseline, not the provenance of
arbitrary supplied files.

The baseline is installed Codex CLI 0.153.4, public source revision
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`. The checked-in fixture is a documented
projection of six generated schemas with original file hashes, not a complete
schema or captured live conversation.

## Native contract blockers

The [App Server API](https://learn.chatgpt.com/docs/app-server) provides native
thread/turn events and account rate-limit reads, but the reviewed implementation
does not expose the required conditional failed-turn continuation:

- [`turn/start`](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/app-server/src/request_processors/turn_processor.rs)
  uses start-or-steer without an expected failed predecessor or user-intent
  revision. `turn/steer` requires an active turn, so cannot resume a failed one.
- [`thread/queue`](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/ext/queue/src/service.rs)
  can wake dispatch on enqueue. Queue start is not conditional on the failed
  predecessor or durable pause/cancel intent. Neither queue IDs nor
  `clientUserMessageId` establish durable replay-safe submission.
- Core has internal `recover_turn_if_idle`, but the generated public request
  interface exposes no corresponding recovery RPC.
- Native rate-limit reads can fetch fresh backend data and may include
  `accountId`; the response does not atomically bind a later continuation to
  that account, session ownership, failed turn, and unchanged user intent.

Before any opt-in actuator is implemented, it needs an authenticated binding to
the existing session, durable task consent and pause/cancel/question holds,
conditional submission against the exact interrupted turn, crash-safe replay,
fresh same-account availability, and native progress evidence. It must bound
continuations and back off on repeated quota without replacing sessions or
provider routes. None of those mutation guarantees are inferred from screen
text or idle snapshots. There is no enable switch, timer, or deployment step
for this groundwork.
