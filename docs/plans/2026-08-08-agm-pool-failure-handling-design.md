# AGM Pool Failure Handling Design

Date: 2026-08-08

## Context

Sub2 uses Antigravity Manager (AGM) as a single upstream account while AGM
manages the real Antigravity account pool. A failure from one AGM account must
therefore be handled inside AGM. Exposing an internal account failure to Sub2
causes the outer gateway to treat the whole AGM pool as limited.

Production evidence showed two final `429` responses for
`gemini-2.5-pro`. In both cases the real upstream status was `503` with
`No capacity available for model gemini-2.5-pro on the server`. AGM retried the
same account with long backoff and then converted the terminal result to `429`.
The requests took 72-83 seconds. The account files still listed
`gemini-2.5-pro` for all nine accounts, with seven accounts reporting more than
90 percent quota, so this was provider capacity rather than unsupported-model
or whole-pool quota exhaustion.

## Goals

- Keep internal account-level failures inside AGM whenever another account can
  serve the request.
- Prevent one failed account from being selected repeatedly in the same request
  or in subsequent requests during its cooldown.
- Preserve the real terminal upstream status instead of converting every
  exhausted retry loop to `429`.
- Return provider/model capacity failures quickly without poisoning account
  health.
- Apply consistent behavior to Gemini-native, OpenAI-compatible, and Claude/CC
  request paths.
- Require no Sub2 code or configuration changes.

## Non-Goals

- Do not remap `gemini-2.5-pro` to a different model.
- Do not hide genuine whole-pool exhaustion.
- Do not implement response caching or change prompt-cache behavior.
- Do not change account credentials, user data, billing, or Sub2 scheduling.

## Error Classification

AGM will classify retryable upstream failures into four scopes:

1. `account_auth`: verification, permission, and account-risk-control errors.
2. `account_model`: quota exhaustion or rate limiting for one account/model.
3. `provider_model`: server capacity unavailable for the requested model.
4. `transport`: connection, timeout, and endpoint failures.

Classification uses the HTTP status plus known structured error fields and
messages. Unknown errors retain their original status and use conservative
retry behavior.

## Retry And Rotation

Each request maintains an attempt state containing tried account IDs, the last
upstream status and body, and the overall retry deadline.

- An account is attempted at most once per request, except for an explicitly
  bounded grace retry when the provider asks for a delay of at most two seconds.
- Account-scoped `401`, `403`, and `429` failures mark the account or its model
  as unavailable and rotate immediately to a distinct eligible account.
- Account-model cooldown uses the provider reset delay when available and the
  existing bounded fallback otherwise.
- Verification/risk-control failures block the account from proxy scheduling
  until validation succeeds or the existing account recovery path clears it.
- Pool traversal may try each eligible account once, up to a hard safety cap of
  ten accounts and an overall retry deadline. The current nine-account pool is
  fully covered.
- Provider-model `503` capacity errors do not mark or rotate accounts. The
  upstream client's existing endpoint fallback is allowed to complete, followed
  by at most one short retry when useful.
- No retry delay runs after the final permitted attempt.
- Transport failures retain bounded retries and never become account quota
  failures.

## Terminal Responses

AGM records the last real status, headers, and sanitized error body.

- Return `429` only when all eligible accounts are unavailable because of
  account-scoped rate or quota limits.
- Return `503` for provider/model capacity exhaustion.
- Preserve other meaningful upstream statuses when no recovery path succeeds.
- Return `502` only for transport/protocol failures that have no upstream HTTP
  status.
- Preserve `Retry-After` when available.

The public error body remains protocol-compatible. Internal logs add error
scope, attempted account count, terminal status, and whether the pool was
actually exhausted. No access token or credential is logged.

## Handler Integration

A shared pool-attempt helper will own classification, attempted-account state,
cooldown updates, terminal status selection, and retry-budget checks. Protocol
handlers retain request/response mapping and streaming conversion.

Gemini-native is migrated first because it currently has the observed bug.
OpenAI-compatible and Claude/CC handlers then use the same classification and
terminal-response rules so the pool cannot behave differently by endpoint.

Streaming handlers must decide whether to retry before writing semantic response
bytes. Once a client response is committed, they emit a protocol-correct stream
error and record the real upstream scope without attempting unsafe failover.

## Testing

Unit and handler tests cover:

- First account returns account-level `429`, second account succeeds.
- Failed account is not selected twice in one request.
- Account-model cooldown excludes the account from a subsequent request.
- Verification failure blocks only the affected account.
- Provider capacity `503` returns `503`, does not mark an account, and completes
  without the previous long backoff.
- All accounts return account-level `429`, producing one terminal `429` after
  each eligible account is attempted at most once.
- Final retry attempts do not sleep.
- Transport-only failure returns `502` rather than `429`.
- Gemini-native, OpenAI-compatible, and Claude/CC endpoints produce consistent
  terminal status and pool-exhaustion behavior.

Production verification will compare AGM request logs before and after rollout:
terminal status by model, request duration, attempted-account count, and any
remaining AGM-originated `429` responses. Sub2 account state must remain active
through single-account and provider-capacity failures.

## Rollout

1. Add failing tests for the observed Gemini behavior.
2. Implement shared classification and attempt-state primitives.
3. Migrate Gemini-native and verify locally.
4. Apply the shared behavior to OpenAI-compatible and Claude/CC handlers.
5. Build a versioned AGM image and deploy it to `144.225.124.79`.
6. Run controlled success, account-level `429`, and provider-capacity `503`
   probes while monitoring logs.
7. Keep the previous container image available for rollback.

