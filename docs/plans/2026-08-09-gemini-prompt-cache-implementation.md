# Gemini Prompt Cache Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add observable, failure-safe prompt cache reuse to AGM's native Gemini request path and verify it against `gemini-2.5-flash`.

**Architecture:** Extend the existing `CacheManager` with account/project/model-scoped keys and connect it to the native Gemini wrapper and handler. Cache metadata remains process-local; cache rejection retries once without cache, and ordinary generation remains the fallback.

**Tech Stack:** Rust, Axum, Tokio, serde_json, existing AGM `CacheManager`, Cargo tests, Docker production smoke tests.

---

### Task 1: Native Gemini Cache Key And Injection

**Files:**
- Modify: `src-tauri/src/proxy/cache_manager.rs`
- Modify: `src-tauri/src/proxy/mappers/gemini/wrapper.rs`

1. Add failing tests for deterministic keys, account/project/model isolation, and native request injection.
2. Run the focused tests and confirm they fail for missing behavior.
3. Implement the smallest scoped-key and injection helpers.
4. Run the focused tests and existing cache-manager tests.
5. Commit the change.

### Task 2: Cache Lifecycle And Failure Fallback

**Files:**
- Modify: `src-tauri/src/proxy/handlers/gemini.rs`
- Modify: `src-tauri/src/proxy/cache_manager.rs`
- Test: colocated Rust tests in the modified modules

1. Add failing tests for cache hit lookup, one-hour expiry, and stale-cache retry without injection.
2. Confirm the tests fail for the intended missing behavior.
3. Connect cache lookup/injection to each selected account and remove rejected entries before one uncached retry.
4. Ensure cache failures never replace a successful ordinary request path.
5. Run Gemini handler, cache-manager, and pool-retry tests, then commit.

### Task 3: Usage And Monitoring

**Files:**
- Modify: `src-tauri/src/proxy/middleware/monitor.rs`
- Modify: `src-tauri/src/proxy/mappers/gemini/collector.rs`
- Modify only if required: `src-tauri/src/proxy/monitor.rs`

1. Add failing tests for `cachedContentTokenCount` extraction from normal and streaming Gemini responses.
2. Implement extraction through existing `cached_tokens` fields without changing public response semantics.
3. Run monitor, collector, OpenAI mapper, and token-stat tests.
4. Commit the change.

### Task 4: Full Verification And Flash Probe

**Files:**
- No production source changes unless verification exposes a defect.

1. Run formatting checks and all focused Gemini/cache/retry/rate-limit/token tests.
2. Build a dedicated Docker image and smoke-test it on an isolated port.
3. Deploy with a compose backup and automatic rollback.
4. Send repeated stable-prefix `gemini-2.5-flash` requests and inspect upstream usage for positive cached tokens.
5. Verify health, account count, model count, restart count, and public endpoint health.
