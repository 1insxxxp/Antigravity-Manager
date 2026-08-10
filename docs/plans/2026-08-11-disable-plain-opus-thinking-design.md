# Disable Plain Opus Thinking Design

## Problem

Plain `claude-opus-4-6` aliases currently resolve to the supported upstream
`claude-opus-4-6-thinking` model and are assigned an enabled thinking config.
The response mapper hides thinking blocks, but the upstream still spends
reasoning tokens. The Claude request mapper further raises the effective Opus
thinking budget to 24576, so response filtering does not solve token usage.

## Behavior

- Plain aliases (`claude-opus-4-6`, `claude-opus-4.6`, and
  `claude-opus-4-6-20260201`) continue to use the compatible upstream model ID,
  but explicitly disable thinking with `thinkingBudget: 0` and
  `includeThoughts: false`.
- Any client thinking hint is ignored for a plain alias. Users who want
  thinking must select `claude-opus-4-6-thinking` explicitly.
- Historical thinking and redacted-thinking blocks are removed before the
  request is transformed so they do not become input text.
- Explicit `claude-opus-4-6-thinking` behavior remains unchanged.
- Response filtering remains as a defensive guard in case the upstream emits
  an unexpected thought block while disabled.

## Implementation

Derive an Opus thinking policy from the original requested model before model
mapping. Apply that policy after variant resolution so the mapped upstream ID
can remain `claude-opus-4-6-thinking`. Represent disabled thinking explicitly
in the Claude request and teach generation-config construction to emit the
zero-budget config for that state. Remove historical thinking blocks only for
the plain policy.

## Verification

- Unit tests cover alias classification, forced disabled request state,
  zero-budget generation config, history removal, and unchanged explicit
  thinking behavior.
- Production tests inspect SSE block types and raw upstream usage metadata.
  Plain requests must return text without thought blocks and report zero
  thoughts tokens; explicit thinking requests must still emit thinking.

