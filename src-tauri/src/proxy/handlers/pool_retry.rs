use once_cell::sync::Lazy;
use regex::Regex;
use std::{collections::HashSet, time::Duration};

const MAX_ACCOUNT_ATTEMPTS: usize = 10;
const MAX_SANITIZED_ERROR_CHARS: usize = 1_024;

static REQUEST_BODY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)\b(request\s+(?:body|payload))\s*[:=].*$")
        .expect("request body regex must compile")
});
static BEARER_CREDENTIAL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(bearer|basic)\s+[a-z0-9._~+/=-]+")
        .expect("authorization credential regex must compile")
});
static SECRET_ASSIGNMENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)("?(?:access[_-]?token|refresh[_-]?token|id[_-]?token|api[_-]?key|x-api-key|client[_-]?secret|password|authorization|proxy-authorization)"?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,;}]+)"#,
    )
    .expect("secret assignment regex must compile")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolFailureScope {
    AccountAuth,
    AccountModel,
    ProviderModel,
    Transport,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDisposition {
    RotateAccount,
    GraceRetry(Duration),
    Return,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiRetryAction {
    CooldownAndRotate,
    ReturnProviderStatus,
    ReturnFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRetryAction {
    CooldownAndRotate,
    ReturnProviderStatus,
    ReturnFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolFailure {
    pub status: u16,
    pub scope: PoolFailureScope,
    pub disposition: RetryDisposition,
    pub retry_after: Option<String>,
    pub sanitized_error: String,
}

impl PoolFailure {
    /// Represents a failure that occurred before an HTTP response was received.
    pub fn transport(error_text: &str) -> Self {
        Self {
            status: 0,
            scope: PoolFailureScope::Transport,
            disposition: RetryDisposition::Return,
            retry_after: None,
            sanitized_error: sanitize_error(error_text),
        }
    }

    pub fn should_cooldown_account(&self) -> bool {
        matches!(
            self.scope,
            PoolFailureScope::AccountAuth | PoolFailureScope::AccountModel
        )
    }
}

#[derive(Debug, Clone)]
pub struct PoolAttemptState {
    attempted_account_ids: HashSet<String>,
    account_limit_account_ids: HashSet<String>,
    pool_size: usize,
    max_account_attempts: usize,
    last_failure: Option<PoolFailure>,
}

impl PoolAttemptState {
    pub fn new(pool_size: usize) -> Self {
        Self {
            attempted_account_ids: HashSet::new(),
            account_limit_account_ids: HashSet::new(),
            pool_size,
            max_account_attempts: pool_size.min(MAX_ACCOUNT_ATTEMPTS),
            last_failure: None,
        }
    }

    pub fn record_failure(&mut self, account_id: impl Into<String>, failure: PoolFailure) {
        let account_id = account_id.into();
        self.attempted_account_ids.insert(account_id.clone());
        if failure.should_cooldown_account() {
            self.account_limit_account_ids.insert(account_id);
        }

        let has_meaningful_http_failure = self
            .last_failure
            .as_ref()
            .is_some_and(|last| last.scope != PoolFailureScope::Transport);
        if failure.scope != PoolFailureScope::Transport || !has_meaningful_http_failure {
            self.last_failure = Some(failure);
        }
    }

    pub fn attempted_account_ids(&self) -> &HashSet<String> {
        &self.attempted_account_ids
    }

    pub fn max_account_attempts(&self) -> usize {
        self.max_account_attempts
    }

    pub fn last_failure(&self) -> Option<&PoolFailure> {
        self.last_failure.as_ref()
    }

    pub fn account_limit_failure_count(&self) -> usize {
        self.account_limit_account_ids.len()
    }

    pub fn whole_pool_exhausted(&self) -> bool {
        self.pool_size > 0
            && self.pool_size <= MAX_ACCOUNT_ATTEMPTS
            && self.attempted_account_ids.len() >= self.pool_size
            && self.account_limit_account_ids.len() == self.attempted_account_ids.len()
    }

    pub fn terminal_status(&self) -> u16 {
        if self.whole_pool_exhausted() {
            return 429;
        }

        if !self.attempted_account_ids.is_empty()
            && self.account_limit_account_ids.len() == self.attempted_account_ids.len()
        {
            return 503;
        }

        match self.last_failure.as_ref() {
            Some(failure)
                if failure.scope != PoolFailureScope::Transport && failure.status != 0 =>
            {
                failure.status
            }
            _ => 502,
        }
    }

    pub fn retry_after(&self) -> Option<&str> {
        self.last_failure
            .as_ref()
            .and_then(|failure| failure.retry_after.as_deref())
    }
}

pub fn classify_pool_failure(
    status: u16,
    error_text: &str,
    retry_after: Option<&str>,
) -> PoolFailure {
    let normalized = error_text.to_ascii_lowercase();
    let scope = if matches!(status, 401 | 403) || is_account_auth_failure(&normalized) {
        PoolFailureScope::AccountAuth
    } else if status == 429 && is_account_model_failure(&normalized) {
        PoolFailureScope::AccountModel
    } else if status == 503 && is_provider_model_failure(&normalized) {
        PoolFailureScope::ProviderModel
    } else {
        PoolFailureScope::Unknown
    };

    let disposition = match scope {
        PoolFailureScope::AccountAuth | PoolFailureScope::AccountModel => {
            RetryDisposition::RotateAccount
        }
        PoolFailureScope::ProviderModel
        | PoolFailureScope::Transport
        | PoolFailureScope::Unknown => RetryDisposition::Return,
    };

    PoolFailure {
        status,
        scope,
        disposition,
        retry_after: retry_after.map(str::to_owned),
        sanitized_error: sanitize_error(error_text),
    }
}

pub fn gemini_retry_action(
    failure: &PoolFailure,
    remaining_account_attempts: usize,
) -> GeminiRetryAction {
    match failure.scope {
        PoolFailureScope::AccountAuth | PoolFailureScope::AccountModel
            if remaining_account_attempts > 0 =>
        {
            GeminiRetryAction::CooldownAndRotate
        }
        PoolFailureScope::ProviderModel => GeminiRetryAction::ReturnProviderStatus,
        PoolFailureScope::AccountAuth
        | PoolFailureScope::AccountModel
        | PoolFailureScope::Transport
        | PoolFailureScope::Unknown => GeminiRetryAction::ReturnFailure,
    }
}

pub fn openai_retry_action(
    failure: &PoolFailure,
    remaining_account_attempts: usize,
) -> OpenAiRetryAction {
    match failure.scope {
        PoolFailureScope::AccountAuth | PoolFailureScope::AccountModel
            if remaining_account_attempts > 0 =>
        {
            OpenAiRetryAction::CooldownAndRotate
        }
        PoolFailureScope::ProviderModel => OpenAiRetryAction::ReturnProviderStatus,
        PoolFailureScope::AccountAuth
        | PoolFailureScope::AccountModel
        | PoolFailureScope::Transport
        | PoolFailureScope::Unknown => OpenAiRetryAction::ReturnFailure,
    }
}

fn is_account_auth_failure(normalized: &str) -> bool {
    const MARKERS: &[&str] = &[
        "verify your account",
        "account verification",
        "validation_required",
        "validation_url",
        "validationurl",
        "validation url",
        "appeal_url",
        "further action is required",
        "risk-control",
        "risk_control",
        "risk control",
        "permission denied",
        "insufficient permissions",
        "account suspended",
        "account disabled",
    ];

    MARKERS.iter().any(|marker| normalized.contains(marker))
}

fn is_account_model_failure(normalized: &str) -> bool {
    const MARKERS: &[&str] = &[
        "quota",
        "rate limit",
        "rate_limit",
        "resource exhausted",
        "resource_exhausted",
        "capacity on this model",
        "account capacity",
        "reset after",
        "retry after",
    ];

    MARKERS.iter().any(|marker| normalized.contains(marker))
}

fn is_provider_model_failure(normalized: &str) -> bool {
    normalized.contains("no capacity available for model")
        || normalized.contains("model_capacity_exhausted")
}

fn sanitize_error(error_text: &str) -> String {
    let without_request_body = REQUEST_BODY_RE.replace(error_text, "$1: [REDACTED]");
    let without_bearer = BEARER_CREDENTIAL_RE.replace_all(&without_request_body, "$1 [REDACTED]");
    let sanitized = SECRET_ASSIGNMENT_RE.replace_all(&without_bearer, "$1[REDACTED]");

    if sanitized.chars().count() <= MAX_SANITIZED_ERROR_CHARS {
        return sanitized.into_owned();
    }

    let mut truncated: String = sanitized
        .chars()
        .take(MAX_SANITIZED_ERROR_CHARS - 3)
        .collect();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_model_capacity_is_not_account_limit() {
        let failure = classify_pool_failure(
            503,
            "No capacity available for model gemini-2.5-pro on the server",
            None,
        );

        assert_eq!(failure.status, 503);
        assert_eq!(failure.scope, PoolFailureScope::ProviderModel);
        assert_eq!(failure.disposition, RetryDisposition::Return);
        assert!(!failure.should_cooldown_account());
    }

    #[test]
    fn structured_model_capacity_is_provider_scoped() {
        let failure = classify_pool_failure(503, r#"{"reason":"MODEL_CAPACITY_EXHAUSTED"}"#, None);

        assert_eq!(failure.scope, PoolFailureScope::ProviderModel);
        assert_eq!(failure.disposition, RetryDisposition::Return);
    }

    #[test]
    fn account_quota_429_rotates_and_cools_model() {
        let failure = classify_pool_failure(
            429,
            "You have exhausted your capacity on this model. Your quota will reset after 4h59m58s.",
            Some("17998"),
        );

        assert_eq!(failure.status, 429);
        assert_eq!(failure.scope, PoolFailureScope::AccountModel);
        assert_eq!(failure.disposition, RetryDisposition::RotateAccount);
        assert!(failure.should_cooldown_account());
        assert_eq!(failure.retry_after.as_deref(), Some("17998"));
    }

    #[test]
    fn gemini_account_429_marks_and_rotates() {
        let failure = classify_pool_failure(429, "quota will reset after 4h", None);

        assert_eq!(
            gemini_retry_action(&failure, 9),
            GeminiRetryAction::CooldownAndRotate
        );
    }

    #[test]
    fn gemini_provider_capacity_503_returns_without_long_backoff() {
        let failure = classify_pool_failure(503, "No capacity available for model", None);

        assert_eq!(
            gemini_retry_action(&failure, 9),
            GeminiRetryAction::ReturnProviderStatus
        );
    }

    #[test]
    fn openai_account_429_marks_and_rotates() {
        let failure = classify_pool_failure(429, "quota will reset after 4h", None);

        assert_eq!(
            openai_retry_action(&failure, 9),
            OpenAiRetryAction::CooldownAndRotate
        );
    }

    #[test]
    fn openai_provider_capacity_503_preserves_provider_status() {
        let failure = classify_pool_failure(503, "No capacity available for model", None);

        assert_eq!(
            openai_retry_action(&failure, 9),
            OpenAiRetryAction::ReturnProviderStatus
        );
    }

    #[test]
    fn openai_account_failure_stops_when_no_accounts_remain() {
        let failure = classify_pool_failure(429, "quota will reset after 4h", None);

        assert_eq!(
            openai_retry_action(&failure, 0),
            OpenAiRetryAction::ReturnFailure
        );
    }

    #[test]
    fn verification_failure_blocks_only_the_account() {
        let failure = classify_pool_failure(429, "Verify your account to continue.", None);

        assert_eq!(failure.scope, PoolFailureScope::AccountAuth);
        assert_eq!(failure.disposition, RetryDisposition::RotateAccount);
        assert!(failure.should_cooldown_account());
    }

    #[test]
    fn validation_required_is_account_auth() {
        let failure = classify_pool_failure(
            403,
            r#"{"reason":"VALIDATION_REQUIRED","validationUrl":"https://example.invalid"}"#,
            None,
        );

        assert_eq!(failure.scope, PoolFailureScope::AccountAuth);
        assert_eq!(failure.disposition, RetryDisposition::RotateAccount);
    }

    #[test]
    fn authentication_statuses_are_account_scoped_without_marker_text() {
        for status in [401, 403] {
            let failure = classify_pool_failure(status, "upstream authentication failed", None);

            assert_eq!(failure.scope, PoolFailureScope::AccountAuth);
            assert_eq!(failure.disposition, RetryDisposition::RotateAccount);
        }
    }

    #[test]
    fn terminal_status_preserves_provider_503() {
        let mut state = PoolAttemptState::new(9);
        state.record_failure(
            "acc-1",
            classify_pool_failure(503, "No capacity available for model gemini-2.5-pro", None),
        );

        assert_eq!(state.terminal_status(), 503);
    }

    #[test]
    fn account_attempt_cap_is_pool_size_or_ten() {
        assert_eq!(PoolAttemptState::new(0).max_account_attempts(), 0);
        assert_eq!(PoolAttemptState::new(4).max_account_attempts(), 4);
        assert_eq!(PoolAttemptState::new(10).max_account_attempts(), 10);
        assert_eq!(PoolAttemptState::new(25).max_account_attempts(), 10);
    }

    #[test]
    fn attempted_account_ids_are_unique() {
        let mut state = PoolAttemptState::new(2);
        let account_limit = || {
            classify_pool_failure(
                429,
                "Account quota exhausted; quota will reset after 1h.",
                None,
            )
        };

        state.record_failure("acc-1", account_limit());
        state.record_failure("acc-1", account_limit());

        assert_eq!(state.attempted_account_ids().len(), 1);
        assert!(state.attempted_account_ids().contains("acc-1"));
        assert_eq!(state.account_limit_failure_count(), 1);
    }

    #[test]
    fn whole_pool_exhaustion_requires_every_eligible_account_limit() {
        let mut state = PoolAttemptState::new(2);
        state.record_failure(
            "acc-1",
            classify_pool_failure(429, "Account quota exhausted; reset after 1h.", None),
        );
        assert!(!state.whole_pool_exhausted());

        state.record_failure(
            "acc-2",
            classify_pool_failure(429, "Model quota exhausted; reset after 2h.", None),
        );
        assert!(state.whole_pool_exhausted());
        assert_eq!(state.terminal_status(), 429);
    }

    #[test]
    fn capped_attempts_do_not_claim_a_larger_pool_is_exhausted() {
        let mut state = PoolAttemptState::new(11);

        for index in 0..10 {
            state.record_failure(
                format!("acc-{index}"),
                classify_pool_failure(
                    429,
                    "Account quota exhausted; quota will reset after 1h.",
                    None,
                ),
            );
        }

        assert!(!state.whole_pool_exhausted());
        assert_eq!(state.terminal_status(), 503);
    }

    #[test]
    fn later_account_limit_updates_the_same_attempted_account() {
        let mut state = PoolAttemptState::new(1);
        state.record_failure(
            "acc-1",
            classify_pool_failure(500, "temporary upstream failure", None),
        );
        state.record_failure(
            "acc-1",
            classify_pool_failure(429, "Account quota exhausted; reset after 1h.", None),
        );

        assert_eq!(state.account_limit_failure_count(), 1);
        assert!(state.whole_pool_exhausted());
    }

    #[test]
    fn non_account_failure_prevents_whole_pool_exhaustion() {
        let mut state = PoolAttemptState::new(2);
        state.record_failure(
            "acc-1",
            classify_pool_failure(429, "Account quota exhausted; reset after 1h.", None),
        );
        state.record_failure(
            "acc-2",
            classify_pool_failure(503, "No capacity available for model gemini-2.5-pro", None),
        );

        assert!(!state.whole_pool_exhausted());
        assert_eq!(state.terminal_status(), 503);
    }

    #[test]
    fn transport_only_terminal_failure_returns_502() {
        let mut state = PoolAttemptState::new(1);
        state.record_failure("acc-1", PoolFailure::transport("connection reset by peer"));

        assert_eq!(state.terminal_status(), 502);
    }

    #[test]
    fn retry_after_is_preserved_from_final_meaningful_failure() {
        let mut state = PoolAttemptState::new(2);
        state.record_failure(
            "acc-1",
            classify_pool_failure(
                429,
                "Account quota exhausted; quota will reset after 120 seconds.",
                Some("120"),
            ),
        );
        state.record_failure("acc-2", PoolFailure::transport("connection reset by peer"));

        assert_eq!(state.retry_after(), Some("120"));
        assert_eq!(state.terminal_status(), 429);
    }

    #[test]
    fn unknown_failures_preserve_status_and_return() {
        let failure = classify_pool_failure(418, "upstream returned an unfamiliar error", None);

        assert_eq!(failure.status, 418);
        assert_eq!(failure.scope, PoolFailureScope::Unknown);
        assert_eq!(failure.disposition, RetryDisposition::Return);
    }

    #[test]
    fn stored_errors_are_sanitized_and_truncated() {
        let error = format!(
            "Authorization: Bearer secret-token request body: {{\"password\":\"hunter2\"}} {}",
            "x".repeat(2_000)
        );
        let failure = classify_pool_failure(500, &error, None);

        assert!(!failure.sanitized_error.contains("secret-token"));
        assert!(!failure.sanitized_error.contains("hunter2"));
        assert!(!failure.sanitized_error.contains("request body: {"));
        assert!(failure.sanitized_error.chars().count() <= 1_024);
    }
}
