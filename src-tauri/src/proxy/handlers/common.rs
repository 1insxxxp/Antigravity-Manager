use crate::proxy::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::time::{sleep, Duration};
use tracing::{debug, info};

// ===== 统一重试与退避策略 =====

/// 重试策略枚举
#[derive(Debug, Clone)]
pub enum RetryStrategy {
    /// 不重试，直接返回错误
    NoRetry,
    /// 固定延迟
    FixedDelay(Duration),
    /// 线性退避：base_ms * (attempt + 1)
    LinearBackoff { base_ms: u64 },
    /// 指数退避：base_ms * 2^attempt，上限 max_ms
    ExponentialBackoff { base_ms: u64, max_ms: u64 },
    /// [NEW] 原地重试 (Grace Retry)：在当前账号上小窗口等待后直接重试，不计入常规切换
    GraceRetry(Duration),
}

/// 根据错误状态码和错误信息确定重试策略
pub fn determine_retry_strategy(
    status_code: u16,
    error_text: &str,
    retried_without_thinking: bool,
) -> RetryStrategy {
    match status_code {
        // Google Cloud Code may reject an otherwise valid account on one regional edge.
        // Rotate promptly so another account can be selected.
        400 if is_unsupported_location_error(error_text) => {
            RetryStrategy::FixedDelay(Duration::from_millis(200))
        }

        // 400 错误：仅在特定 Thinking 签名失败时重试一次
        400 if !retried_without_thinking
            && (error_text.contains("Invalid `signature`")
                || error_text.contains("thinking.signature")
                || error_text.contains("thinking.thinking")
                || error_text.contains("Corrupted thought signature")) =>
        {
            RetryStrategy::FixedDelay(Duration::from_millis(200))
        }

        // 429 限流错误
        429 => {
            // 优先使用服务端返回的 Retry-After / quotaResetDelay
            if let Some(delay_ms) = crate::proxy::upstream::retry::parse_retry_delay(error_text) {
                // [NEW] 如果延迟在 2s 内，执行 Grace Retry (原地重试)
                if crate::proxy::upstream::retry::should_grace_retry(delay_ms) {
                    let actual_delay = delay_ms.saturating_add(100); // 增加 100ms 安全缓冲
                    tracing::info!(
                        "Grace Retry Triggered: Delay {}ms is within window, using same account",
                        actual_delay
                    );
                    RetryStrategy::GraceRetry(Duration::from_millis(actual_delay))
                } else {
                    let actual_delay = delay_ms.saturating_add(200).min(30_000);
                    RetryStrategy::FixedDelay(Duration::from_millis(actual_delay))
                }
            } else {
                // 否则使用线性退避：起始 5s，逐步增加
                RetryStrategy::LinearBackoff { base_ms: 5000 }
            }
        }

        // 模型容量不足通常是账号/节点相关，短暂等待后立即轮换账号
        503 if is_model_capacity_error(error_text) => {
            RetryStrategy::FixedDelay(Duration::from_millis(500))
        }

        // 503 服务不可用 / 529 服务器过载
        503 | 529 => {
            // 指数退避：起始 10s，上限 60s (针对 Google 边缘节点过载)
            RetryStrategy::ExponentialBackoff {
                base_ms: 10000,
                max_ms: 60000,
            }
        }

        // 500 服务器内部错误
        500 => {
            // 线性退避：起始 3s
            RetryStrategy::LinearBackoff { base_ms: 3000 }
        }

        // 401/403 认证/权限错误：切换账号前给予极短缓冲
        401 | 403 => RetryStrategy::FixedDelay(Duration::from_millis(200)),

        // 404 资源未找到：Google Cloud Code API 的 404 通常是账号级别的间歇性问题
        // (灰度发布、账号权限不同步等)，轮换账号往往能解决
        404 => RetryStrategy::FixedDelay(Duration::from_millis(300)),

        // 其他错误：不重试
        _ => RetryStrategy::NoRetry,
    }
}

/// 执行退避策略并返回是否应该继续重试
pub async fn apply_retry_strategy(
    strategy: RetryStrategy,
    attempt: usize,
    max_attempts: usize,
    status_code: u16,
    trace_id: &str,
) -> bool {
    match strategy {
        RetryStrategy::NoRetry => {
            debug!(
                "[{}] Non-retryable error {}, stopping",
                trace_id, status_code
            );
            false
        }

        RetryStrategy::FixedDelay(duration) => {
            let base_ms = duration.as_millis() as u64;
            info!(
                "[{}] ⏱️ Retry with fixed delay: status={}, attempt={}/{}, delay={}ms",
                trace_id,
                status_code,
                attempt + 1,
                max_attempts,
                base_ms
            );
            sleep(duration).await;
            true
        }

        RetryStrategy::LinearBackoff { base_ms } => {
            let calculated_ms = base_ms * (attempt as u64 + 1);
            info!(
                "[{}] ⏱️ Retry with linear backoff: status={}, attempt={}/{}, delay={}ms",
                trace_id,
                status_code,
                attempt + 1,
                max_attempts,
                calculated_ms
            );
            sleep(Duration::from_millis(calculated_ms)).await;
            true
        }

        RetryStrategy::ExponentialBackoff { base_ms, max_ms } => {
            let calculated_ms = (base_ms * 2_u64.pow(attempt as u32)).min(max_ms);
            info!(
                "[{}] ⏱️ Retry with exponential backoff: status={}, attempt={}/{}, delay={}ms",
                trace_id,
                status_code,
                attempt + 1,
                max_attempts,
                calculated_ms
            );
            sleep(Duration::from_millis(calculated_ms)).await;
            true
        }

        RetryStrategy::GraceRetry(duration) => {
            info!(
                "[{}] ⚡ Grace Retry: Performing micro-wait ({}ms) on current account...",
                trace_id,
                duration.as_millis()
            );
            sleep(duration).await;
            true // 原地重试在 handlers 层面通过 should_rotate_account 判断是否切换
        }
    }
}

/// 判断是否应该轮换账号
pub fn should_rotate_account(status_code: u16, strategy: Option<&RetryStrategy>) -> bool {
    // [NEW] 如果识别为 Grace Retry，则显式要求不轮换账号
    if let Some(RetryStrategy::GraceRetry(_)) = strategy {
        return false;
    }

    match status_code {
        // 这些错误是账号级别或特定节点配额的，需要轮换
        429 | 401 | 403 | 404 | 500 => true,
        // 503/529 通常是后端过载，切号效果有限，暂不轮换
        503 | 529 => false,
        _ => false,
    }
}

fn value_contains_model_capacity_error(value: &Value) -> bool {
    match value {
        Value::String(text) => {
            let normalized = text.to_ascii_lowercase();
            normalized.contains("model_capacity_exhausted")
                || normalized.contains("image_capacity_exhausted")
                || normalized.contains("no capacity available for model")
        }
        Value::Array(items) => items.iter().any(value_contains_model_capacity_error),
        Value::Object(fields) => fields.values().any(value_contains_model_capacity_error),
        _ => false,
    }
}

pub fn is_model_capacity_error(error_text: &str) -> bool {
    let json_start = error_text.find('{').unwrap_or(0);
    if let Ok(value) = serde_json::from_str::<Value>(&error_text[json_start..]) {
        if value_contains_model_capacity_error(&value) {
            return true;
        }
    }

    let normalized = error_text.to_ascii_lowercase();
    normalized.contains("model_capacity_exhausted")
        || normalized.contains("image_capacity_exhausted")
        || normalized.contains("no capacity available for model")
}

pub fn should_rotate_account_for_error(
    status_code: u16,
    strategy: Option<&RetryStrategy>,
    error_text: &str,
) -> bool {
    if status_code == 400 && is_unsupported_location_error(error_text) {
        return true;
    }

    if status_code == 503 && is_model_capacity_error(error_text) {
        return true;
    }

    should_rotate_account(status_code, strategy)
}

fn is_unsupported_location_error(error_text: &str) -> bool {
    error_text
        .to_ascii_lowercase()
        .contains("user location is not supported")
}

pub fn exhausted_response_status(last_status: Option<StatusCode>, last_error: &str) -> StatusCode {
    if last_status == Some(StatusCode::SERVICE_UNAVAILABLE) && is_model_capacity_error(last_error) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::TOO_MANY_REQUESTS
    }
}

/// Track accounts that must not be selected again during the same request.
pub fn record_account_for_rotation(
    should_rotate: bool,
    account_id: &str,
    attempted_account_ids: &mut HashSet<String>,
) -> bool {
    if should_rotate {
        attempted_account_ids.insert(account_id.to_string());
    }
    should_rotate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn records_only_accounts_that_must_rotate() {
        let mut attempted_account_ids = HashSet::new();

        assert!(record_account_for_rotation(
            true,
            "failed-account",
            &mut attempted_account_ids,
        ));
        assert!(attempted_account_ids.contains("failed-account"));

        assert!(!record_account_for_rotation(
            false,
            "retry-same-account",
            &mut attempted_account_ids,
        ));
        assert!(!attempted_account_ids.contains("retry-same-account"));
    }

    #[test]
    fn rotates_account_for_structured_model_capacity_503() {
        let error = r#"{
            "error": {
                "code": 503,
                "message": "No capacity available for model gemini-3.1-flash-image on the server",
                "details": [{"reason": "MODEL_CAPACITY_EXHAUSTED"}]
            }
        }"#;

        assert!(should_rotate_account_for_error(503, None, error));
    }

    #[test]
    fn rotates_account_for_image_capacity_503() {
        let error = r#"{"error":{"code":503,"details":[{"reason":"IMAGE_CAPACITY_EXHAUSTED"}]}}"#;

        assert!(should_rotate_account_for_error(503, None, error));
    }

    #[test]
    fn keeps_same_account_for_generic_503() {
        let error = r#"{"error":{"code":503,"message":"Service temporarily unavailable"}}"#;

        assert!(!should_rotate_account_for_error(503, None, error));
    }

    #[test]
    fn quickly_retries_model_capacity_503() {
        let error = "HTTP 503: No capacity available for model gemini-3.1-flash-image";

        match determine_retry_strategy(503, error, false) {
            RetryStrategy::FixedDelay(delay) => {
                assert_eq!(delay, Duration::from_millis(500));
            }
            strategy => panic!("unexpected retry strategy: {strategy:?}"),
        }
    }

    #[test]
    fn retries_and_rotates_for_unsupported_location() {
        let error = r#"{"error":{"code":400,"message":"User location is not supported for the API use.","status":"FAILED_PRECONDITION"}}"#;

        assert!(matches!(
            determine_retry_strategy(400, error, false),
            RetryStrategy::FixedDelay(_)
        ));
        assert!(should_rotate_account_for_error(400, None, error));
    }

    #[test]
    fn does_not_retry_unrelated_bad_request() {
        let error = r#"{"error":{"code":400,"status":"INVALID_ARGUMENT"}}"#;

        assert!(matches!(
            determine_retry_strategy(400, error, false),
            RetryStrategy::NoRetry
        ));
        assert!(!should_rotate_account_for_error(400, None, error));
    }

    #[test]
    fn preserves_service_unavailable_for_exhausted_model_capacity() {
        let error = "HTTP 503: No capacity available for model gemini-3.1-flash-image";

        assert_eq!(
            exhausted_response_status(Some(StatusCode::SERVICE_UNAVAILABLE), error),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

/// Detects model capabilities and configuration
/// POST /v1/models/detect
pub async fn handle_detect_model(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let model_name = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

    if model_name.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing 'model' field").into_response();
    }

    // 1. Resolve mapping
    let mapped_model = crate::proxy::common::model_mapping::resolve_model_route(
        model_name,
        &*state.custom_mapping.read().await,
    );

    // 2. Resolve capabilities
    let config = crate::proxy::mappers::common_utils::resolve_request_config(
        model_name,
        &mapped_model,
        &None, // We don't check tools for static capability detection
        None,  // size
        None,  // quality
        None,  // image_size
        None,  // body (not needed for static detection)
    );

    // 3. Construct response
    let mut response = json!({
        "model": model_name,
        "mapped_model": mapped_model,
        "type": config.request_type,
        "features": {
            "has_web_search": config.inject_google_search,
            "is_image_gen": config.request_type == "image_gen"
        }
    });

    if let Some(img_conf) = config.image_config {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("config".to_string(), img_conf);
        }
    }

    Json(response).into_response()
}
