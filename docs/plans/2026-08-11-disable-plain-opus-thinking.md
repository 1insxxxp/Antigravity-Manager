# Disable Plain Opus Thinking Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Stop upstream reasoning-token generation for plain Opus 4.6 aliases while preserving explicit Opus thinking.

**Architecture:** Classify the original client model before variant mapping, carry an explicit disabled-thinking request state through the Claude mapper, and emit a zero-budget upstream generation config. Strip historical thinking blocks only for plain aliases and retain response filtering as a defensive guard.

**Tech Stack:** Rust, serde_json, Axum, Cargo tests, Docker.

---

### Task 1: Classify Plain And Explicit Opus Requests

**Files:**
- Modify: `src-tauri/src/proxy/handlers/claude.rs`

1. Add failing tests showing plain aliases resolve to disabled policy and the
   explicit thinking model resolves to enabled policy.
2. Run the focused handler tests and confirm they fail for missing policy.
3. Implement the minimal policy helper based on the original requested model.
4. Re-run the focused tests and confirm they pass.

### Task 2: Build A Truly Disabled Upstream Request

**Files:**
- Modify: `src-tauri/src/proxy/handlers/claude.rs`
- Modify: `src-tauri/src/proxy/mappers/claude/request.rs`

1. Add failing tests asserting plain Opus produces
   `thinkingConfig.includeThoughts=false` and `thinkingBudget=0` after request
   transformation, regardless of a client-provided thinking budget.
2. Add a failing test asserting explicit Opus thinking retains its enabled
   budget behavior.
3. Run the focused request tests and confirm the expected failures.
4. Apply the policy after variant mapping and extend generation config handling
   for an explicit disabled state.
5. Re-run the focused tests and confirm they pass.

### Task 3: Remove Historical Thinking Input

**Files:**
- Modify: `src-tauri/src/proxy/handlers/claude.rs`

1. Add a failing unit test with text, thinking, redacted thinking, and tool
   blocks showing only thinking blocks are removed for plain Opus.
2. Implement a narrow history-cleaning helper.
3. Re-run the focused tests and confirm they pass.

### Task 4: Regression Verification And Delivery

**Files:**
- Test: `src-tauri/src/proxy/handlers/claude.rs`
- Test: `src-tauri/src/proxy/mappers/claude/request.rs`
- Test: `src-tauri/src/proxy/mappers/claude/response.rs`
- Test: `src-tauri/src/proxy/mappers/claude/streaming.rs`

1. Run focused Claude handler, request, response, and streaming tests.
2. Run `cargo check --bin antigravity_tools` with the shared target directory.
3. Run `git diff --check` and review the final diff.
4. Commit and push the change.
5. Build and deploy a rollback-safe production image.
6. Verify plain and explicit model behavior plus raw thoughts-token usage,
   health, restart count, and recent errors.

