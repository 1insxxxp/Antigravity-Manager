# Account Model Test Design

## Goal

Add a one-click diagnostic for an imported account that sends a minimal model request with that account only and reports the real upstream result.

## User Experience

Each account card and table row gains a test action. The action opens a modal containing the models reported in that account's quota data. The user selects one model and starts the test. The modal then shows success or failure, HTTP status, elapsed time, selected model, and a short response or upstream error.

The test must never fall back to another account. This makes 401, 403, 429, and 503 responses useful diagnostics instead of hiding them behind pool failover.

## Architecture

The account module exposes one shared asynchronous test function. It loads and refreshes the selected account credentials, resolves its project ID, constructs a minimal `generateContent` request, and sends it through the existing upstream client. A Tauri command and a Web API route call the same function.

The React account service provides a typed wrapper. Account cards and table rows open a dedicated modal, which derives available models from the selected account quota and renders a stable result view.

## Result Contract

The backend returns a serializable result containing:

- account ID and email
- requested model
- success flag
- HTTP status
- elapsed milliseconds
- short response text on success
- sanitized error text on failure

Transport failures return the same result shape when possible. Credential loading and validation failures are returned as command/API errors.

## Safety

- The request uses only the selected account.
- The prompt is a minimal deterministic health check.
- Access tokens and other credentials are never returned or logged.
- Response and error previews are length-limited.
- The action is disabled for accounts already disabled in the proxy.

## Testing

Backend unit tests cover request construction, preview truncation, and response classification. Frontend tests cover model option extraction and result formatting where the existing test setup permits. The final verification includes Rust tests, frontend type checking/build, and a production smoke test through the Web API.
