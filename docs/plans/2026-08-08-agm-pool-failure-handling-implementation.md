# AGM Pool Failure Handling Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Keep internal Antigravity account failures inside AGM, preserve real provider status codes, and expose `429` only when the usable account pool is genuinely exhausted.

**Architecture:** Add a protocol-neutral pool-failure classifier and per-request attempt state, then make each protocol handler use it for cooldown, rotation, and terminal status selection. Extend `TokenManager` with explicit account exclusions so a failed account cannot be selected twice in one request; keep the existing public token-selection API as a compatibility wrapper.

**Tech Stack:** Rust, Axum, Tokio, DashMap-backed `TokenManager`, Reqwest, Cargo unit tests, Docker backend image.

---

### Task 1: Add Pool Failure Classification And Attempt State

**Files:**
- Create: `src-tauri/src/proxy/handlers/pool_retry.rs`
- Modify: `src-tauri/src/proxy/handlers/mod.rs`
- Test: `src-tauri/src/proxy/handlers/pool_retry.rs`

**Step 1: Write the failing classification tests**

Add tests covering at least these inputs:

```rust
#[test]
fn provider_model_capacity_is_not_account_limit() {
    let failure = classify_pool_failure(
        503,
        "No capacity available for model gemini-2.5-pro on the server",
        None,
    );
    assert_eq!(failure.scope, PoolFailureScope::ProviderModel);
    assert_eq!(failure.disposition, RetryDisposition::Return);
    assert!(!failure.should_cooldown_account());
}

#[test]
fn account_quota_429_rotates_and_cools_model() {
    let failure = classify_pool_failure(
        429,
        "You have exhausted your capacity on this model. Your quota will reset after 4h59m58s.",
        None,
    );
    assert_eq!(failure.scope, PoolFailureScope::AccountModel);
    assert_eq!(failure.disposition, RetryDisposition::RotateAccount);
    assert!(failure.should_cooldown_account());
}

#[test]
fn verification_failure_blocks_only_the_account() {
    let failure = classify_pool_failure(429, "Verify your account to continue.", None);
    assert_eq!(failure.scope, PoolFailureScope::AccountAuth);
    assert_eq!(failure.disposition, RetryDisposition::RotateAccount);
}

#[test]
fn terminal_status_preserves_provider_503() {
    let mut state = PoolAttemptState::new(9);
    state.record_failure("acc-1", classify_pool_failure(503, "No capacity available", None));
    assert_eq!(state.terminal_status(), 503);
}
```

Also test that `PoolAttemptState`:

- caps account attempts at `min(pool_size, 10)`;
- stores unique account IDs;
- reports whole-pool exhaustion only when all eligible attempts ended in account-scoped limits;
- returns `502` for a transport-only terminal failure;
- preserves `Retry-After` from the final meaningful failure.

**Step 2: Run the tests and verify they fail**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
```

Expected: FAIL because `pool_retry` and its types do not exist.

**Step 3: Implement the minimal shared types**

Implement:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolFailureScope {
    AccountAuth,
    AccountModel,
    ProviderModel,
    Transport,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDisposition {
    RotateAccount,
    GraceRetry(Duration),
    Return,
}

#[derive(Debug, Clone)]
pub struct PoolFailure {
    pub status: u16,
    pub scope: PoolFailureScope,
    pub disposition: RetryDisposition,
    pub retry_after: Option<String>,
    pub sanitized_error: String,
}

pub struct PoolAttemptState {
    attempted_account_ids: HashSet<String>,
    max_account_attempts: usize,
    last_failure: Option<PoolFailure>,
    account_limit_failures: usize,
}
```

Keep classification deterministic and conservative:

- `Verify your account`, validation URLs, and explicit permission/risk-control text -> `AccountAuth`.
- `429` plus quota/reset/account-capacity text -> `AccountModel`.
- `503` plus `No capacity available for model` or structured `MODEL_CAPACITY_EXHAUSTED` -> `ProviderModel`.
- network errors without an HTTP response -> `Transport`.
- unknown statuses preserve their original status and default to `Return` unless an existing protocol-specific recovery explicitly applies.

Sanitize and truncate stored errors; do not store request bodies, access tokens, or credentials.

**Step 4: Export the module and run the tests**

Add `pub mod pool_retry;` to `src-tauri/src/proxy/handlers/mod.rs`.

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
```

Expected: PASS.

**Step 5: Commit**

```bash
git add src-tauri/src/proxy/handlers/mod.rs src-tauri/src/proxy/handlers/pool_retry.rs
git commit -m "feat: classify AGM pool failures"
```

### Task 2: Prevent Retry Delay After The Final Attempt

**Files:**
- Modify: `src-tauri/src/proxy/handlers/common.rs`
- Modify: `src-tauri/src/proxy/tests/retry_strategy_tests.rs`

**Step 1: Write the failing boundary test**

Add a pure helper test so the test does not actually sleep:

```rust
#[test]
fn final_attempt_has_no_retry_slot() {
    assert!(has_retry_attempt_remaining(0, 3));
    assert!(has_retry_attempt_remaining(1, 3));
    assert!(!has_retry_attempt_remaining(2, 3));
    assert!(!has_retry_attempt_remaining(0, 0));
}
```

**Step 2: Verify the test fails**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml retry_strategy_tests --lib
```

Expected: FAIL because `has_retry_attempt_remaining` does not exist.

**Step 3: Implement the boundary and use it before sleeping**

Add:

```rust
pub fn has_retry_attempt_remaining(attempt: usize, max_attempts: usize) -> bool {
    attempt.saturating_add(1) < max_attempts
}
```

At the start of `apply_retry_strategy`, return `false` when no retry slot remains. Do not alter the existing per-status delay calculation in this task.

**Step 4: Run focused tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml retry_strategy_tests --lib
```

Expected: PASS.

**Step 5: Commit**

```bash
git add src-tauri/src/proxy/handlers/common.rs src-tauri/src/proxy/tests/retry_strategy_tests.rs
git commit -m "fix: skip backoff after final retry attempt"
```

### Task 3: Add Explicit Account Exclusions To Token Selection

**Files:**
- Modify: `src-tauri/src/proxy/token_manager.rs`
- Test: `src-tauri/src/proxy/token_manager.rs`

**Step 1: Write failing token-selection tests**

Using the existing temporary account-file test pattern, create three enabled accounts that support the same model. Assert that:

```rust
let excluded = HashSet::from(["acc-1".to_string(), "acc-2".to_string()]);
let (_, _, _, selected_id, _) = manager
    .get_token_excluding("gemini", true, Some("sid"), "gemini-2.5-pro", &excluded)
    .await
    .unwrap();
assert_eq!(selected_id, "acc-3");
```

Add tests for sticky-session exclusion and for a clear error when every eligible account is excluded.

**Step 2: Verify the tests fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml get_token_excluding --lib
```

Expected: FAIL because the new API does not exist.

**Step 3: Implement a compatibility-preserving API**

Add:

```rust
pub async fn get_token_excluding(
    &self,
    quota_group: &str,
    force_rotate: bool,
    session_id: Option<&str>,
    target_model: &str,
    excluded_account_ids: &HashSet<String>,
) -> Result<(String, String, String, String, u64), String>
```

Filter the token snapshot by account ID before preferred-account, sticky-session, and round-robin selection. If a sticky binding points to an excluded account, remove that binding and continue selection.

Keep the existing `get_token` signature and make it delegate with an empty exclusion set so unrelated callers do not change.

**Step 4: Run token-manager tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml proxy::token_manager::tests --lib
```

Expected: PASS.

**Step 5: Commit**

```bash
git add src-tauri/src/proxy/token_manager.rs
git commit -m "feat: exclude attempted accounts from pool selection"
```

### Task 4: Migrate Gemini-Native Requests To Pool-Aware Failover

**Files:**
- Modify: `src-tauri/src/proxy/handlers/gemini.rs`
- Modify: `src-tauri/src/proxy/handlers/pool_retry.rs`
- Test: `src-tauri/src/proxy/handlers/pool_retry.rs`
- Test: `src-tauri/src/proxy/tests/retry_strategy_tests.rs`

**Step 1: Add failing Gemini policy tests**

Add tests for a protocol policy function that returns the next action without making network calls:

```rust
#[test]
fn gemini_account_429_marks_and_rotates() {
    let failure = classify_pool_failure(429, "quota will reset after 4h", None);
    assert_eq!(gemini_retry_action(&failure, 9), GeminiRetryAction::CooldownAndRotate);
}

#[test]
fn gemini_provider_capacity_503_returns_without_long_backoff() {
    let failure = classify_pool_failure(503, "No capacity available for model", None);
    assert_eq!(gemini_retry_action(&failure, 9), GeminiRetryAction::ReturnProviderStatus);
}
```

Also assert that terminal `503` stays `503` and terminal account-pool exhaustion becomes `429`.

**Step 2: Run focused tests and verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
```

Expected: FAIL because the Gemini policy is not implemented.

**Step 3: Replace the hard-coded three-account loop**

In `handle_generate`:

- create `PoolAttemptState::new(pool_size)`;
- obtain tokens through `get_token_excluding`;
- record the selected account before sending upstream;
- allow up to `min(pool_size, 10)` distinct account attempts;
- retain the existing at-most-two-second grace retry without adding the account to the exclusion set twice;
- stop using the unconditional terminal `StatusCode::TOO_MANY_REQUESTS` block.

**Step 4: Apply failure-specific behavior**

- `AccountModel`: call `mark_rate_limited_async` with the mapped model, clear/override sticky selection through account exclusion, and rotate immediately without the existing 5/10/15-second delay.
- `AccountAuth`: reuse validation/forbidden helpers where applicable, exclude the account, and rotate.
- `ProviderModel`: do not call `mark_rate_limited_async`; allow the upstream client's endpoint fallback, then return the real `503` without account rotation or long handler backoff.
- `Transport`: retain bounded transport retry and return `502` if no upstream HTTP response exists.
- Unknown non-retryable errors: return the original status and protocol-compatible Google error body.

Preserve `Retry-After` when constructing the terminal response. Add structured logs for scope, attempt count, max attempts, terminal status, and pool exhaustion; continue masking account email.

**Step 5: Run focused and full proxy tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
cargo test --manifest-path src-tauri/Cargo.toml retry_strategy_tests --lib
cargo test --manifest-path src-tauri/Cargo.toml proxy::tests --lib
```

Expected: PASS.

**Step 6: Commit**

```bash
git add src-tauri/src/proxy/handlers/gemini.rs src-tauri/src/proxy/handlers/pool_retry.rs src-tauri/src/proxy/tests/retry_strategy_tests.rs
git commit -m "fix: keep Gemini account limits inside AGM"
```

### Task 5: Align OpenAI-Compatible Pool Failover

**Files:**
- Modify: `src-tauri/src/proxy/handlers/openai.rs`
- Modify: `src-tauri/src/proxy/handlers/pool_retry.rs`
- Test: `src-tauri/src/proxy/handlers/pool_retry.rs`

**Step 1: Add failing OpenAI policy tests**

Test that OpenAI-compatible routes use the same account exclusion and terminal status rules, including the chat/responses paths that currently end with unconditional `429`.

**Step 2: Run and verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
```

Expected: FAIL on the new OpenAI policy assertions.

**Step 3: Integrate the shared attempt state**

For each main OpenAI retry loop:

- replace `MAX_RETRY_ATTEMPTS = 3` account traversal with the shared pool cap;
- pass attempted account IDs to `get_token_excluding`;
- mark only account-scoped limits;
- do not mark provider-model `503` as an account limit;
- replace unconditional final `429` responses with `PoolAttemptState::terminal_status()`;
- keep signature-repair retries separate from account traversal so they do not consume another account unless the repair fails with an account-scoped error.

Do not refactor unrelated Codex guidance, mapper, or streaming code.

**Step 4: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
cargo test --manifest-path src-tauri/Cargo.toml proxy::tests --lib
```

Expected: PASS.

**Step 5: Commit**

```bash
git add src-tauri/src/proxy/handlers/openai.rs src-tauri/src/proxy/handlers/pool_retry.rs
git commit -m "fix: align OpenAI pool failover status"
```

### Task 6: Align Claude And CC Pool Failover

**Files:**
- Modify: `src-tauri/src/proxy/handlers/claude.rs`
- Modify: `src-tauri/src/proxy/handlers/pool_retry.rs`
- Test: `src-tauri/src/proxy/handlers/pool_retry.rs`

**Step 1: Add failing Claude policy tests**

Cover account-model `429`, validation `403`, provider-model `503`, and the existing Claude-specific behavior that maps terminal authentication failure away from a client-login redirect. Ensure provider capacity remains `503` and never becomes `429`.

**Step 2: Run and verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
```

Expected: FAIL on the new Claude policy assertions.

**Step 3: Integrate shared pool behavior**

- use explicit attempted-account exclusions;
- preserve the existing thinking-signature repair path as a same-account repair;
- keep validation blocking for the affected account only;
- mark only account-scoped model limits;
- preserve real terminal status and `Retry-After`;
- do not retry after semantic stream bytes have been committed.

**Step 4: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml pool_retry --lib
cargo test --manifest-path src-tauri/Cargo.toml proxy::tests --lib
```

Expected: PASS.

**Step 5: Commit**

```bash
git add src-tauri/src/proxy/handlers/claude.rs src-tauri/src/proxy/handlers/pool_retry.rs
git commit -m "fix: align Claude pool failover status"
```

### Task 7: Verify, Build, Deploy, And Probe Production

**Files:**
- Modify only if required by test findings: files from Tasks 1-6
- Operational target: `144.225.124.79`, container `antigravity-manager`

**Step 1: Format and run the complete Rust verification set**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo test --manifest-path src-tauri/Cargo.toml --lib
cargo clippy --manifest-path src-tauri/Cargo.toml --lib -- -D warnings
```

Expected: all commands exit 0. If pre-existing Clippy warnings prevent `-D warnings`, record the exact baseline and verify no new warnings in touched files.

**Step 2: Review the final diff for scope and secrets**

```bash
git diff HEAD~6 --check
git diff HEAD~6 --stat
git status --short
```

Expected: only planned Rust files and plan documents changed; no account JSON, API key, token, database, or local configuration is present.

**Step 3: Build a versioned Linux backend image**

Use the existing backend Dockerfile and current frontend image:

```bash
docker buildx build --platform linux/amd64 \
  -f docker/Dockerfile.backend \
  --build-arg FRONTEND_IMAGE=lbjlaq/antigravity-manager:v4.5.1 \
  -t antigravity-manager:pool-failover-20260808 \
  --load .
```

Expected: image builds successfully and contains `/app/antigravity-tools`.

**Step 4: Back up the live deployment metadata and load the image**

Before changing the container, record its image ID, mounts, environment names, restart policy, and published ports without printing secret values. Keep the current v4.5.1 image locally for rollback. Transfer/load the new image using the existing secure SSH path.

**Step 5: Replace only the AGM container**

Recreate `antigravity-manager` with the same data mount, port `8045`, environment, and restart policy. Do not modify the card containers or `/opt/antigravity-manager/data`.

Expected health result:

```json
{"status":"ok"}
```

**Step 6: Run controlled probes**

Without printing API keys or account tokens, verify:

- a normal supported Gemini request returns `200`;
- a controlled provider-capacity response remains `503` and does not create an account cooldown;
- an account-level limit rotates to another eligible account when one exists;
- no terminal request sleeps after its final attempt;
- AGM request logs show the real terminal status and attempt count;
- Sub2 account `5107` remains active and has no `rate_limit_reset_at`, `overload_until`, or `temp_unschedulable_until` after a provider `503`.

Compare production duration with the observed 72-83-second baseline.

**Step 7: Commit any verification-only correction**

Only if verification required a code correction:

```bash
git add <exact corrected files>
git commit -m "fix: address pool failover verification findings"
```

Otherwise leave the verified implementation commits unchanged.

