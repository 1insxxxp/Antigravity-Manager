# Opus 4.6 Thinking Visibility Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Hide upstream thinking for plain Opus 4.6 requests while preserving it for explicit thinking requests.

**Architecture:** Derive a visibility boolean from the original model name in the Claude handler. Thread it through streaming and non-streaming response processors so thought parts are ignored only for plain Opus 4.6 aliases.

**Tech Stack:** Rust, Axum, Anthropic SSE, Cargo tests

---

### Task 1: Define the visibility policy

**Files:**
- Modify: `src-tauri/src/proxy/handlers/claude.rs`

1. Add tests for plain and explicit-thinking Opus 4.6 model names.
2. Run the focused tests and confirm they fail because the policy helper is absent.
3. Add the minimal helper that hides thinking only for plain Opus 4.6 aliases.
4. Run the focused tests and confirm they pass.

### Task 2: Filter non-streaming thinking blocks

**Files:**
- Modify: `src-tauri/src/proxy/mappers/claude/response.rs`
- Modify: `src-tauri/src/proxy/handlers/claude.rs`

1. Add a failing response-conversion test containing one thought part and one text part.
2. Add a visibility parameter to `NonStreamingProcessor` and `transform_response`.
3. Ignore thought content and thought-only signatures when visibility is disabled.
4. Run the response mapper tests and confirm both hidden and exposed cases pass.

### Task 3: Filter streaming thinking events

**Files:**
- Modify: `src-tauri/src/proxy/mappers/claude/streaming.rs`
- Modify: `src-tauri/src/proxy/mappers/claude/mod.rs`
- Modify: `src-tauri/src/proxy/handlers/claude.rs`

1. Add a failing streaming test that processes thought and text parts with thinking hidden.
2. Add the visibility flag to `StreamingState` and the stream factory.
3. Skip thought parts and their signatures while hidden, without setting `has_thinking`.
4. Run focused streaming tests and confirm explicit thinking remains unchanged.

### Task 4: Verify and deploy

**Files:**
- No additional source files.

1. Run focused Claude mapper and handler tests.
2. Run `cargo check --manifest-path src-tauri/Cargo.toml --bin antigravity_tools`.
3. Commit and push the branch.
4. Build a production image from the verified commit and preserve the current container for rollback.
5. Verify health, plain-model SSE visibility, explicit-thinking SSE visibility, and restart count.
