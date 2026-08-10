# Account Model Test Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a per-account, per-model upstream diagnostic to AGM account management.

**Architecture:** A shared Rust function performs a minimal upstream `generateContent` call with a specifically selected account. Thin Tauri and Axum adapters expose it, while React provides a typed service and a model-selection result modal from every account view.

**Tech Stack:** Rust, Tokio, Reqwest, Axum, Tauri, React, TypeScript, Vitest

---

### Task 1: Backend Diagnostic Core

**Files:**
- Modify: `src-tauri/src/modules/account.rs`

1. Add failing unit tests for the minimal request body, bounded response previews, and HTTP success/error classification.
2. Run the focused Rust tests and verify they fail because the diagnostic helpers do not exist.
3. Add the serializable result type and helper functions.
4. Implement the async account/model test using refreshed credentials, project resolution, and the existing upstream client.
5. Run the focused tests and the account module test suite.

### Task 2: Tauri and Web API Adapters

**Files:**
- Modify: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/src/modules/http_api.rs`
- Modify: `src-tauri/src/lib.rs`

1. Add the `test_account_model` Tauri command delegating to the shared core.
2. Add `POST /accounts/{id}/test` with a typed JSON model request.
3. Register both adapters.
4. Run Rust compilation and tests.

### Task 3: Frontend Service and Modal

**Files:**
- Modify: `src/utils/request.ts`
- Modify: `src/services/accountService.ts`
- Create: `src/components/accounts/AccountTestModal.tsx`
- Modify: `src/pages/Accounts.tsx`
- Modify: `src/components/accounts/AccountCard.tsx`
- Modify: `src/components/accounts/AccountGrid.tsx`
- Modify: `src/components/accounts/AccountTable.tsx`
- Modify: `src/components/accounts/AccountRow.tsx`
- Modify: `src/locales/zh-CN.json`
- Modify: `src/locales/en.json`

1. Add the command-to-Web-API mapping and typed service function.
2. Add a test action to card and table variants.
3. Build a compact modal with model selection, loading state, and structured result display.
4. Connect the selected account from `Accounts.tsx`.
5. Run frontend type checking and build.

### Task 4: End-to-End Verification and Deployment

**Files:**
- Modify only deployment image references on the server.

1. Run focused Rust tests, full relevant Rust tests, and the frontend build.
2. Build a production container while the current stable container remains live.
3. Deploy the new image and verify container health and restart count.
4. Exercise the Web API account-test route without exposing credentials.
5. Commit and push the implementation.
