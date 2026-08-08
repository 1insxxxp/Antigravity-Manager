// Gemini Handler
use axum::{
    extract::State,
    extract::{Json, Path},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};
use std::collections::HashSet;
use tracing::{debug, error, info};

use crate::proxy::common::client_adapter::CLIENT_ADAPTERS;
use crate::proxy::debug_logger;
use crate::proxy::handlers::common::{
    apply_retry_strategy, determine_retry_strategy, RetryStrategy,
};
use crate::proxy::handlers::pool_retry::{
    classify_pool_failure, gemini_retry_action, GeminiRetryAction, PoolAttemptState, PoolFailure,
    PoolFailureScope,
};
use crate::proxy::mappers::gemini::{unwrap_response, wrap_request, wrap_request_v2};
use crate::proxy::server::AppState;
use crate::proxy::session_manager::SessionManager;
use crate::proxy::upstream::client::mask_email;

#[derive(Clone)]
struct GeminiAccountContext {
    access_token: String,
    project_id: String,
    email: String,
    account_id: String,
}

fn gemini_error_response(
    status_code: u16,
    error_text: &str,
    email: Option<&str>,
    mapped_model: Option<&str>,
    retry_after: Option<&str>,
) -> Response {
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (
        status,
        Json(json!({
            "error": {
                "code": status.as_u16(),
                "message": error_text,
                "status": "UPSTREAM_ERROR"
            }
        })),
    )
        .into_response();

    if let Some(email) = email.and_then(|value| HeaderValue::from_str(value).ok()) {
        response.headers_mut().insert("x-account-email", email);
    }
    if let Some(model) = mapped_model.and_then(|value| HeaderValue::from_str(value).ok()) {
        response.headers_mut().insert("x-mapped-model", model);
    }
    if let Some(retry_after) = retry_after.and_then(|value| HeaderValue::from_str(value).ok()) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after);
    }

    response
}

/// 处理 generateContent 和 streamGenerateContent
/// 路径参数: model_name, method (e.g. "gemini-pro", "generateContent")
pub async fn handle_generate(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,          // [NEW] Extract headers for adapter detection
    Json(mut body): Json<Value>, // 改为 mut 以支持修复提示词注入
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // 解析 model:method
    let (model_name, method) = if let Some((m, action)) = model_action.rsplit_once(':') {
        (m.to_string(), action.to_string())
    } else {
        (model_action, "generateContent".to_string())
    };

    crate::modules::logger::log_info(&format!(
        "Received Gemini request: {}/{}",
        model_name, method
    ));
    let trace_id = format!("req_{}", chrono::Utc::now().timestamp_subsec_millis());
    let debug_cfg = state.debug_logging.read().await.clone();

    // [NEW] Detect Client Adapter
    let client_adapter = CLIENT_ADAPTERS
        .iter()
        .find(|a| a.matches(&headers))
        .cloned();
    if client_adapter.is_some() {
        debug!("[{}] Client Adapter detected", trace_id);
    }

    // 1. 验证方法
    if method != "generateContent" && method != "streamGenerateContent" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Unsupported method: {}", method),
        ));
    }
    if debug_logger::is_enabled(&debug_cfg) {
        let original_payload = json!({
            "kind": "original_request",
            "protocol": "gemini",
            "trace_id": trace_id,
            "original_model": model_name,
            "method": method,
            "request": body.clone(),
        });
        debug_logger::write_debug_payload(
            &debug_cfg,
            Some(&trace_id),
            "original_request",
            &original_payload,
        )
        .await;
    }
    let client_wants_stream = method == "streamGenerateContent";
    // [AUTO-CONVERSION] 强制内部流式化
    let force_stream_internally = !client_wants_stream;
    let is_stream = client_wants_stream || force_stream_internally;

    if force_stream_internally {
        // debug!("[AutoConverter] Converting non-stream request to stream");
    }

    // 2. 获取 UpstreamClient 和 TokenManager
    let upstream = state.upstream.clone();
    let token_manager = state.token_manager;
    let initial_mapped_model = crate::proxy::common::model_mapping::resolve_model_route(
        &model_name,
        &*state.custom_mapping.read().await,
    );
    let pool_size = token_manager
        .eligible_account_count(&initial_mapped_model)
        .await;
    let mut pool_attempts = PoolAttemptState::new(pool_size);
    let max_account_attempts = pool_attempts.max_account_attempts().max(1);
    let max_request_attempts = max_account_attempts.saturating_mul(4).max(1);

    let mut last_error = String::new();
    let mut last_email: Option<String> = None;
    let mut last_mapped_model: Option<String> = None;
    let mut force_rotate = false;
    let mut request_attempt = 0usize;
    let mut selected_account_ids = HashSet::new();
    let mut excluded_account_ids = HashSet::new();
    let mut grace_retried_accounts = HashSet::new();
    let mut signature_retried_accounts = HashSet::new();
    let mut transport_retried_accounts = HashSet::new();
    let mut stream_retried_accounts = HashSet::new();
    let mut retry_account: Option<GeminiAccountContext> = None;

    while request_attempt < max_request_attempts {
        if retry_account.is_none() && selected_account_ids.len() >= max_account_attempts {
            break;
        }
        let attempt = request_attempt;
        request_attempt = request_attempt.saturating_add(1);

        // 3. 模型路由解析
        let mapped_model = crate::proxy::common::model_mapping::resolve_model_route(
            &model_name,
            &*state.custom_mapping.read().await,
        );
        // 提取 tools 列表以进行联网探测 (Gemini 风格可能是嵌套的)
        let tools_val: Option<Vec<Value>> =
            body.get("tools").and_then(|t| t.as_array()).map(|arr| {
                let mut flattened = Vec::new();
                for tool_entry in arr {
                    if let Some(decls) = tool_entry
                        .get("functionDeclarations")
                        .and_then(|v| v.as_array())
                    {
                        flattened.extend(decls.iter().cloned());
                    } else {
                        flattened.push(tool_entry.clone());
                    }
                }
                flattened
            });

        let config = crate::proxy::mappers::common_utils::resolve_request_config(
            &model_name,
            &mapped_model,
            &tools_val,
            None,        // size (not applicable for Gemini native protocol)
            None,        // quality
            None,        // [NEW] image_size
            Some(&body), // [NEW] Pass request body for imageConfig parsing
        );

        // 4. 获取 Token (使用准确的 request_type)
        // 提取 SessionId (粘性指纹)
        let session_id = SessionManager::extract_gemini_session_id(&body, &model_name);

        let account = if let Some(account) = retry_account.take() {
            account
        } else {
            let (access_token, project_id, email, account_id, _wait_ms) = match token_manager
                .get_token_excluding(
                    &config.request_type,
                    force_rotate,
                    Some(&session_id),
                    &config.final_model,
                    &excluded_account_ids,
                )
                .await
            {
                Ok(token) => token,
                Err(error) if !selected_account_ids.is_empty() => {
                    last_error = format!("Token error after pool attempts: {error}");
                    break;
                }
                Err(error) => {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("Token error: {}", error),
                    ));
                }
            };

            selected_account_ids.insert(account_id.clone());
            GeminiAccountContext {
                access_token,
                project_id,
                email,
                account_id,
            }
        };
        let GeminiAccountContext {
            access_token,
            project_id,
            email,
            account_id,
        } = account;

        let mapped_model = token_manager
            .resolve_dynamic_model_for_account(&account_id, &mapped_model)
            .await;

        last_email = Some(email.clone());
        last_mapped_model = Some(mapped_model.clone());
        info!(
            "✓ Using account: {} (type: {}, pool_attempt={}/{})",
            mask_email(&email),
            config.request_type,
            selected_account_ids.len(),
            max_account_attempts
        );

        // 5. 包装请求 (project injection)
        // [FIX #765] Pass session_id to wrap_request for signature injection
        // [NEW] 获取完整 Token 对象以注入动态规格 (dynamic > static default > 65535)
        let token_obj = token_manager.get_token_by_id(&account_id);
        let wrapped_body = wrap_request_v2(
            &body,
            &project_id,
            &mapped_model,
            Some(account_id.as_str()),
            Some(&session_id),
            token_obj.as_ref(),
            Some(&token_manager),
        );

        if debug_logger::is_enabled(&debug_cfg) {
            let payload = json!({
                "kind": "v1internal_request",
                "protocol": "gemini",
                "trace_id": trace_id,
                "original_model": model_name,
                "mapped_model": mapped_model,
                "request_type": config.request_type,
                "attempt": attempt,
                "v1internal_request": wrapped_body.clone(),
            });
            debug_logger::write_debug_payload(
                &debug_cfg,
                Some(&trace_id),
                "v1internal_request",
                &payload,
            )
            .await;
        }

        // 5. 上游调用
        let query_string = if is_stream { Some("alt=sse") } else { None };
        let upstream_method = if is_stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };

        // [FIX #1522] Inject Anthropic Beta Headers for Claude models
        let mut extra_headers = std::collections::HashMap::new();
        if mapped_model.to_lowercase().contains("claude") {
            extra_headers.insert("anthropic-beta".to_string(), "claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14".to_string());
            tracing::debug!(
                "[Gemini] Injected Anthropic beta headers for Claude model: {}",
                mapped_model
            );
        }

        let call_result = match upstream
            .call_v1_internal_with_headers(
                upstream_method,
                &access_token,
                wrapped_body,
                query_string,
                extra_headers.clone(),
                Some(account_id.as_str()),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = e.clone();
                debug!(
                    "Gemini transport failed on request attempt {}/{}: {}",
                    attempt + 1,
                    max_request_attempts,
                    e
                );
                if transport_retried_accounts.insert(account_id.clone()) {
                    retry_account = Some(GeminiAccountContext {
                        access_token,
                        project_id,
                        email,
                        account_id,
                    });
                    force_rotate = false;
                    continue;
                }

                let failure = PoolFailure::transport(&e);
                pool_attempts.record_failure(&account_id, failure.clone());
                excluded_account_ids.insert(account_id);
                let remaining = max_account_attempts.saturating_sub(selected_account_ids.len());
                if gemini_retry_action(&failure, remaining) == GeminiRetryAction::RotateAccount {
                    force_rotate = true;
                    continue;
                }
                break;
            }
        };

        // [NEW] 记录端点降级日志到 debug 文件
        if !call_result.fallback_attempts.is_empty() && debug_logger::is_enabled(&debug_cfg) {
            let fallback_entries: Vec<serde_json::Value> = call_result
                .fallback_attempts
                .iter()
                .map(|a| {
                    json!({
                        "endpoint_url": a.endpoint_url,
                        "status": a.status,
                        "error": a.error,
                    })
                })
                .collect();
            let payload = json!({
                "kind": "endpoint_fallback",
                "protocol": "gemini",
                "trace_id": trace_id,
                "original_model": model_name,
                "mapped_model": mapped_model,
                "attempt": attempt,
                "account": mask_email(&email),
                "fallback_attempts": fallback_entries,
            });
            debug_logger::write_debug_payload(
                &debug_cfg,
                Some(&trace_id),
                "endpoint_fallback",
                &payload,
            )
            .await;
        }

        let response = call_result.response;
        // [NEW] 提取实际请求的上游端点 URL，用于日志记录和排查
        let upstream_url = response.url().to_string();
        let status = response.status();

        // [NEW] 提取官方 TraceID
        let cloud_code_trace_id = response
            .headers()
            .get("x-cloudaicompanion-trace-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        if status.is_success() {
            // 6. 响应处理
            if is_stream {
                use axum::body::Body;
                use bytes::{Bytes, BytesMut};
                use futures::StreamExt;

                let meta = json!({
                    "protocol": "gemini",
                    "trace_id": trace_id,
                    "original_model": model_name,
                    "mapped_model": mapped_model,
                    "request_type": config.request_type,
                    "attempt": attempt,
                    "status": status.as_u16(),
                    "upstream_url": upstream_url,
                });
                let mut response_stream = debug_logger::wrap_stream_with_debug(
                    Box::pin(response.bytes_stream()),
                    debug_cfg.clone(),
                    trace_id.clone(),
                    "upstream_response",
                    meta,
                );
                let mut buffer = BytesMut::new();
                let s_id = session_id.clone(); // Clone for stream closure

                // [FIX #859] Implement peek logic for Gemini stream to prevent 0-token 200 OK
                let mut first_chunk = None;
                let mut retry_gemini = false;

                // [NEW] 实施双阶段超时：第一阶段为 FirstChunkTimeout (300s / 5min)
                // 这精准对齐了官方 Worker 在模型冷启动（Initialization）阶段的极度耐心
                match tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    response_stream.next(),
                )
                .await
                {
                    Ok(Some(Ok(bytes))) => {
                        if bytes.is_empty() {
                            tracing::warn!("[Gemini] Empty first chunk received, retrying...");
                            retry_gemini = true;
                        } else {
                            first_chunk = Some(bytes);
                        }
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!("[Gemini] Stream error during peek: {}, retrying...", e);
                        last_error = format!("Stream error: {}", e);
                        retry_gemini = true;
                    }
                    Ok(None) => {
                        tracing::warn!("[Gemini] Stream ended immediately, retrying...");
                        last_error = "Empty response".to_string();
                        retry_gemini = true;
                    }
                    Err(_) => {
                        tracing::warn!("[Gemini] First chunk timeout after 300s, retrying...");
                        last_error = "First chunk timeout".to_string();
                        retry_gemini = true;
                    }
                }

                if retry_gemini {
                    if last_error.is_empty() {
                        last_error = "Empty or incomplete upstream stream".to_string();
                    }
                    if stream_retried_accounts.insert(account_id.clone()) {
                        retry_account = Some(GeminiAccountContext {
                            access_token,
                            project_id,
                            email,
                            account_id,
                        });
                        force_rotate = false;
                        continue;
                    }

                    let failure = PoolFailure::transport(&last_error);
                    pool_attempts.record_failure(&account_id, failure.clone());
                    excluded_account_ids.insert(account_id);
                    let remaining = max_account_attempts.saturating_sub(selected_account_ids.len());
                    if gemini_retry_action(&failure, remaining) == GeminiRetryAction::RotateAccount
                    {
                        force_rotate = true;
                        continue;
                    }
                    break;
                }

                let s_id_for_stream = s_id.clone();
                let model_name_for_stream = mapped_model.clone();
                let stream = async_stream::stream! {
                    let mut first_data = first_chunk;
                    let mut meta_sent = false;

                    loop {
                        // [NEW] 阶段 6.2: 补全 __cloudCodeMeta 响应元数据透传
                        // 官方 Worker 会将 TraceID 作为 SSE 流的第 0 个数据包下发
                        if !meta_sent {
                            if let Some(tid) = &cloud_code_trace_id {
                                let meta_pkg = serde_json::json!({
                                    "__cloudCodeMeta": {
                                        "traceId": tid
                                    }
                                });
                                yield Ok::<Bytes, String>(Bytes::from(format!("data: {}\n\n", serde_json::to_string(&meta_pkg).unwrap())));
                            }
                            meta_sent = true;
                        }

                        let item = if let Some(fd) = first_data.take() {
                            Some(Ok(fd))
                        } else {
                            // [NEW] 第二阶段为 StreamIdleTimeout (300s / 5min)
                            match tokio::time::timeout(std::time::Duration::from_secs(300), response_stream.next()).await {
                                Ok(next_item) => next_item,
                                Err(_) => {
                                    error!("[Gemini-SSE] Idle timeout after 300s, terminating stream");
                                    None
                                }
                            }
                        };

                        let bytes = match item {
                            Some(Ok(b)) => b,
                            Some(Err(e)) => {
                                error!("[Gemini-SSE] Stream error: {}", e);
                                let error_json = serde_json::json!({
                                    "id": &s_id_for_stream,
                                    "object": "chat.completion.chunk",
                                    "model": &model_name_for_stream,
                                    "choices": [
                                        {
                                            "index": 0,
                                            "delta": {
                                                "content": format!("\n[Stream Error] {}", e)
                                            },
                                            "finish_reason": "error"
                                        }
                                    ]
                                });
                                yield Ok::<Bytes, String>(Bytes::from(format!("data: {}\n\n", serde_json::to_string(&error_json).unwrap_or_default())));
                                yield Ok::<Bytes, String>(Bytes::from("data: [DONE]\n\n"));
                                break;
                            }
                            None => break,
                        };

                        debug!("[Gemini-SSE] Received chunk: {} bytes", bytes.len());
                        buffer.extend_from_slice(&bytes);
                        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                            let line_raw = buffer.split_to(pos + 1);
                            if let Ok(line_str) = std::str::from_utf8(&line_raw) {
                                let line = line_str.trim();
                                if line.is_empty() { continue; }

                                if line.starts_with("data: ") {
                                    let json_part = line.trim_start_matches("data: ").trim();
                                    if json_part == "[DONE]" {
                                        yield Ok::<Bytes, String>(Bytes::from("data: [DONE]\n\n"));
                                        continue;
                                    }

                                    match serde_json::from_str::<Value>(json_part) {
                                        Ok(mut json) => {
                                            // [FIX #765] Extract thoughtSignature from stream
                                            let inner_val = if json.get("response").is_some() {
                                                json.get("response")
                                            } else {
                                                Some(&json)
                                            };

                                            if let Some(resp) = inner_val {
                                                if let Some(candidates) = resp.get("candidates").and_then(|c| c.as_array()) {
                                                    for cand in candidates {
                                                        if let Some(parts) = cand.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
                                                            for part in parts {
                                                                if let Some(sig) = part.get("thoughtSignature").and_then(|s| s.as_str()) {
                                                                    crate::proxy::SignatureCache::global()
                                                                        .cache_session_signature(&s_id_for_stream, sig.to_string(), 1);
                                                                    debug!("[Gemini-SSE] Cached signature (len: {}) for session: {}", sig.len(), s_id_for_stream);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            // [FIX #1522] Inject Tool ID into Stream Response
                                            crate::proxy::mappers::gemini::wrapper::inject_ids_to_response(&mut json, &model_name_for_stream);

                                            // Unwrap v1internal response wrapper
                                            if let Some(inner) = json.get_mut("response").map(|v| v.take()) {
                                                let new_line = format!("data: {}\n\n", serde_json::to_string(&inner).unwrap_or_default());
                                                yield Ok::<Bytes, String>(Bytes::from(new_line));
                                            } else {
                                                yield Ok::<Bytes, String>(Bytes::from(format!("data: {}\n\n", serde_json::to_string(&json).unwrap_or_default())));
                                            }
                                        }
                                        Err(e) => {
                                            debug!("[Gemini-SSE] JSON parse error: {}, passing raw line", e);
                                            yield Ok::<Bytes, String>(Bytes::from(format!("{}\n\n", line)));
                                        }
                                    }
                                } else {
                                    // Non-data lines (comments, etc.)
                                    yield Ok::<Bytes, String>(Bytes::from(format!("{}\n\n", line)));
                                }
                            } else {
                                // Non-UTF8 data? Just pass it through or skip
                                debug!("[Gemini-SSE] Non-UTF8 line encountered");
                                yield Ok::<Bytes, String>(line_raw.freeze());
                            }
                        }
                    }
                };

                if client_wants_stream {
                    let body = Body::from_stream(stream);
                    return Ok(Response::builder()
                        .header("Content-Type", "text/event-stream")
                        .header("Cache-Control", "no-cache")
                        .header("Connection", "keep-alive")
                        .header("X-Accel-Buffering", "no")
                        .header("X-Account-Email", &email)
                        .header("X-Mapped-Model", &mapped_model)
                        .body(body)
                        .unwrap()
                        .into_response());
                } else {
                    // Collect to JSON
                    use crate::proxy::mappers::gemini::collector::collect_stream_to_json;
                    match collect_stream_to_json(Box::pin(stream), &s_id).await {
                        Ok(gemini_resp) => {
                            info!(
                                "[{}] ✓ Stream collected and converted to JSON (Gemini)",
                                session_id
                            );
                            let unwrapped = unwrap_response(&gemini_resp);
                            return Ok((
                                StatusCode::OK,
                                [
                                    ("X-Account-Email", email.as_str()),
                                    ("X-Mapped-Model", mapped_model.as_str()),
                                ],
                                Json(unwrapped),
                            )
                                .into_response());
                        }
                        Err(e) => {
                            error!("Stream collection error: {}", e);
                            return Ok((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Stream collection error: {}", e),
                            )
                                .into_response());
                        }
                    }
                }
            }

            let mut gemini_resp: Value = response
                .json()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

            // [FIX #1522] Inject Tool ID into Non-streaming Response
            crate::proxy::mappers::gemini::wrapper::inject_ids_to_response(
                &mut gemini_resp,
                &mapped_model,
            );

            // [FIX #765] Extract thoughtSignature from non-streaming response
            let inner_val = if gemini_resp.get("response").is_some() {
                gemini_resp.get("response")
            } else {
                Some(&gemini_resp)
            };

            if let Some(resp) = inner_val {
                if let Some(candidates) = resp.get("candidates").and_then(|c| c.as_array()) {
                    for cand in candidates {
                        if let Some(parts) = cand
                            .get("content")
                            .and_then(|c| c.get("parts"))
                            .and_then(|p| p.as_array())
                        {
                            for part in parts {
                                if let Some(sig) =
                                    part.get("thoughtSignature").and_then(|s| s.as_str())
                                {
                                    crate::proxy::SignatureCache::global().cache_session_signature(
                                        &session_id,
                                        sig.to_string(),
                                        1,
                                    );
                                    debug!("[Gemini-Response] Cached signature (len: {}) for session: {}", sig.len(), session_id);
                                }
                            }
                        }
                    }
                }
            }

            let unwrapped = unwrap_response(&gemini_resp);
            return Ok((
                StatusCode::OK,
                [
                    ("X-Account-Email", email.as_str()),
                    ("X-Mapped-Model", mapped_model.as_str()),
                ],
                Json(unwrapped),
            )
                .into_response());
        }

        // 处理错误并重试
        let status_code = status.as_u16();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", status_code));
        last_error = format!("HTTP {}: {}", status_code, error_text);
        if debug_logger::is_enabled(&debug_cfg) {
            let payload = json!({
                "kind": "upstream_response_error",
                "protocol": "gemini",
                "trace_id": trace_id,
                "original_model": model_name,
                "mapped_model": mapped_model,
                "request_type": config.request_type,
                "attempt": attempt,
                "status": status_code,
                "upstream_url": upstream_url,
                "account": mask_email(&email),
                "error_text": error_text,
            });
            debug_logger::write_debug_payload(
                &debug_cfg,
                Some(&trace_id),
                "upstream_response_error",
                &payload,
            )
            .await;
        }

        // Thinking 签名修复属于同账号内部重试，不消耗另一个账号。
        if status_code == 400
            && (error_text.contains("Invalid `signature`")
                || error_text.contains("thinking.signature")
                || error_text.contains("Invalid signature")
                || error_text.contains("Corrupted thought signature"))
            && signature_retried_accounts.insert(account_id.clone())
        {
            tracing::warn!(
                "[Gemini] Signature error detected on account {}, retrying without thinking",
                mask_email(&email)
            );

            // 追加修复提示词到请求体的最后一条内容
            if let Some(contents) = body.get_mut("contents").and_then(|v| v.as_array_mut()) {
                if let Some(last_content) = contents.last_mut() {
                    if let Some(parts) =
                        last_content.get_mut("parts").and_then(|v| v.as_array_mut())
                    {
                        parts.push(json!({
                            "text": "\n\n[System Recovery] Your previous output contained an invalid signature. Please regenerate the response without the corrupted signature block."
                        }));
                        tracing::debug!("[Gemini] Appended repair prompt to last content");
                    }
                }
            }

            let _ = apply_retry_strategy(
                RetryStrategy::FixedDelay(std::time::Duration::from_millis(200)),
                0,
                2,
                status_code,
                &trace_id,
            )
            .await;
            retry_account = Some(GeminiAccountContext {
                access_token,
                project_id,
                email,
                account_id,
            });
            force_rotate = false;
            continue;
        }

        // 仅保留至多 2 秒的同账号 Grace Retry；长 Retry-After 直接进入切号。
        let retry_strategy = determine_retry_strategy(status_code, &error_text, false);
        if matches!(retry_strategy, RetryStrategy::GraceRetry(_))
            && grace_retried_accounts.insert(account_id.clone())
        {
            let _ = apply_retry_strategy(retry_strategy, 0, 2, status_code, &trace_id).await;
            retry_account = Some(GeminiAccountContext {
                access_token,
                project_id,
                email,
                account_id,
            });
            force_rotate = false;
            continue;
        }

        let failure = classify_pool_failure(status_code, &error_text, retry_after.as_deref());
        let failure_scope = failure.scope;
        pool_attempts.record_failure(&account_id, failure.clone());

        match failure_scope {
            PoolFailureScope::AccountModel => {
                token_manager
                    .mark_rate_limited_async(
                        &email,
                        status_code,
                        retry_after.as_deref(),
                        &failure.sanitized_error,
                        Some(&mapped_model),
                    )
                    .await;
                excluded_account_ids.insert(account_id.clone());
            }
            PoolFailureScope::AccountAuth => {
                let normalized_error = error_text.to_ascii_lowercase();
                let requires_validation = normalized_error.contains("validation_required")
                    || normalized_error.contains("verify your account")
                    || normalized_error.contains("validation_url")
                    || normalized_error.contains("validationurl");
                if requires_validation {
                    let block_until = chrono::Utc::now().timestamp() + 10 * 60;
                    if let Err(error) = token_manager
                        .set_validation_block_public(
                            &account_id,
                            block_until,
                            &failure.sanitized_error,
                        )
                        .await
                    {
                        tracing::error!(
                            "[Gemini] Failed to set validation block for {}: {}",
                            mask_email(&email),
                            error
                        );
                    }
                } else if status_code == 403 {
                    if let Err(error) = token_manager
                        .set_forbidden(&account_id, &failure.sanitized_error)
                        .await
                    {
                        tracing::error!(
                            "[Gemini] Failed to mark forbidden account {}: {}",
                            mask_email(&email),
                            error
                        );
                    }
                }
                excluded_account_ids.insert(account_id.clone());
            }
            PoolFailureScope::ProviderModel
            | PoolFailureScope::Transport
            | PoolFailureScope::Unknown => {}
        }

        if client_adapter
            .as_ref()
            .is_some_and(|adapter| adapter.let_it_crash() && attempt > 0)
        {
            tracing::warn!(
                "[Gemini] let_it_crash active: returning upstream status after request attempt {}",
                attempt + 1
            );
            return Ok(gemini_error_response(
                status_code,
                &error_text,
                Some(&email),
                Some(&mapped_model),
                retry_after.as_deref(),
            ));
        }

        let remaining_account_attempts =
            max_account_attempts.saturating_sub(selected_account_ids.len());
        match gemini_retry_action(&failure, remaining_account_attempts) {
            GeminiRetryAction::CooldownAndRotate => {
                tracing::warn!(
                    protocol = "gemini",
                    failure_scope = ?failure_scope,
                    account = %mask_email(&email),
                    account_attempts = selected_account_ids.len(),
                    max_account_attempts,
                    "Rotating account after account-scoped upstream failure"
                );
                force_rotate = true;
                continue;
            }
            GeminiRetryAction::RotateAccount => {
                excluded_account_ids.insert(account_id);
                force_rotate = true;
                continue;
            }
            GeminiRetryAction::CooldownAndReturn => {
                return Ok(gemini_error_response(
                    pool_attempts.terminal_status(),
                    &failure.sanitized_error,
                    Some(&email),
                    Some(&mapped_model),
                    pool_attempts.retry_after(),
                ));
            }
            GeminiRetryAction::ReturnProviderStatus => {
                tracing::warn!(
                    protocol = "gemini",
                    failure_scope = ?failure_scope,
                    terminal_status = status_code,
                    "Returning provider-scoped Gemini failure without account cooldown"
                );
                return Ok(gemini_error_response(
                    status_code,
                    &error_text,
                    Some(&email),
                    Some(&mapped_model),
                    retry_after.as_deref(),
                ));
            }
            GeminiRetryAction::ReturnFailure => {
                if matches!(
                    failure_scope,
                    PoolFailureScope::AccountAuth | PoolFailureScope::AccountModel
                ) {
                    break;
                }

                error!(
                    "Gemini upstream non-retryable error {} (scope {:?})",
                    status_code, failure_scope
                );
                return Ok(gemini_error_response(
                    status_code,
                    &error_text,
                    Some(&email),
                    Some(&mapped_model),
                    retry_after.as_deref(),
                ));
            }
        }
    }

    let terminal_status = pool_attempts.terminal_status();
    let terminal_error = pool_attempts
        .last_failure()
        .map(|failure| failure.sanitized_error.as_str())
        .filter(|error| !error.is_empty())
        .unwrap_or({
            if last_error.is_empty() {
                "No eligible AGM account could complete the request"
            } else {
                last_error.as_str()
            }
        });
    let terminal_scope = pool_attempts.last_failure().map(|failure| failure.scope);
    let pool_exhausted = pool_attempts.whole_pool_exhausted();
    tracing::warn!(
        protocol = "gemini",
        failure_scope = ?terminal_scope,
        account_attempts = selected_account_ids.len(),
        max_account_attempts,
        terminal_status,
        pool_exhausted,
        "Gemini pool request terminated"
    );

    Ok(gemini_error_response(
        terminal_status,
        terminal_error,
        last_email.as_deref(),
        last_mapped_model.as_deref(),
        pool_attempts.retry_after(),
    ))
}

pub async fn handle_list_models(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    use crate::proxy::common::model_mapping::get_all_dynamic_models;

    // 获取所有动态模型列表（与 /v1/models 一致）
    let only_raw = *state.only_raw_quota_models.read().await;
    let model_ids =
        get_all_dynamic_models(&state.custom_mapping, Some(&state.token_manager), only_raw).await;

    // 转换为 Gemini API 格式
    let models: Vec<_> = model_ids
        .into_iter()
        .map(|id| {
            json!({
                "name": format!("models/{}", id),
                "version": "001",
                "displayName": id.clone(),
                "description": "",
                "inputTokenLimit": 128000,
                "outputTokenLimit": 8192,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
                "temperature": 1.0,
                "topP": 0.95,
                "topK": 64
            })
        })
        .collect();

    Ok(Json(json!({ "models": models })))
}

pub async fn handle_get_model(Path(model_name): Path<String>) -> impl IntoResponse {
    Json(json!({
        "name": format!("models/{}", model_name),
        "displayName": model_name
    }))
}

pub async fn handle_count_tokens(
    State(state): State<AppState>,
    Path(_model_name): Path<String>,
    Json(_body): Json<Value>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let model_group = "gemini";
    let (_access_token, _project_id, _, _, _wait_ms) = state
        .token_manager
        .get_token(model_group, false, None, "gemini")
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Token error: {}", e),
            )
        })?;

    Ok(Json(json!({"totalTokens": 0})))
}
