# Opus 4.6 Thinking Visibility Design

## Goal

Keep the compatibility route from `claude-opus-4-6` to
`claude-opus-4-6-thinking` while preventing clients that selected the plain
model name from receiving raw thinking blocks.

## Behavior

- Plain Opus 4.6 aliases call the supported thinking upstream but expose only
  text and tool-use content to Anthropic clients.
- Explicit `claude-opus-4-6-thinking` requests continue to expose thinking
  blocks and signature deltas.
- Other Claude and Gemini model behavior remains unchanged.

## Architecture

The Claude handler derives a response visibility policy from the original
client model name before variant resolution. It passes that policy into both
the streaming and non-streaming Claude response converters. The converters
discard upstream thought parts and their signatures when thinking is hidden,
while continuing to process text, tools, finish reasons, and usage.

## Error Handling

Suppressing thinking must not trigger the existing "thinking without content"
recovery path. An upstream response with no visible text still follows the
normal empty-response handling rather than emitting a synthetic thinking
block.

## Verification

Focused Rust tests cover plain and explicit-thinking model names for both
streaming and non-streaming responses. Production verification sends one
request through each model name and inspects only SSE event types, never model
content or credentials.
