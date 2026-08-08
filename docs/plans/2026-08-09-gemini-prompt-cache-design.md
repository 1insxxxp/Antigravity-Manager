# Gemini Prompt Cache Design

> **Status: Superseded by production verification.** Native Gemini implicit
> prompt caching is already active end to end. Repeated long-prefix Flash
> requests returned `cachedContentTokenCount`, AGM persisted it, and Sub2
> recorded it as `cache_read_tokens`. Explicit `cachedContents` lifecycle code
> is therefore unnecessary and would add risk without fixing the observed
> behavior.

## Goal

Enable Prompt Cache for native Gemini requests handled by AGM, so repeated system instructions and tool definitions can be reused by the upstream Gemini service. This is token-prefix caching, not full response caching.

## Scope

- Add cache lifecycle handling to the native Gemini request path.
- Isolate entries by account, Google project, model, and stable prompt/tool prefix.
- Use a one-hour default TTL with lazy expiry and rebuild.
- Preserve ordinary requests when cache operations fail.
- Parse `cachedContentTokenCount` into existing usage and monitoring fields.
- Validate first with `gemini-2.5-flash`.

## Data Flow

1. Normalize the stable prefix containing system instructions, tools, tool configuration, generation configuration, and model.
2. Compute a deterministic key including account and project identity.
3. Look up an unexpired entry in the in-memory cache manager.
4. On a hit, inject `cachedContent` into the upstream Gemini request.
5. On a miss, send the ordinary request and register a supported cache resource when available.
6. Read `cachedContentTokenCount` from response usage metadata and update token statistics.

## Failure Handling

- Cache API failure must never fail the user request.
- Rejected or expired cache names are removed; the request retries once without cache injection.
- Cache state is process-local initially and rebuilds lazily after restart.

## Verification

- Unit tests cover deterministic keys, isolation, TTL expiry, injection, stale-cache fallback, and usage extraction.
- Existing Gemini, retry, rate-limit, and token-manager tests remain green.
- A live Flash request verifies that the deployed upstream accepts cache reuse and returns a positive cached-token count on repetition.
