use std::fmt;

use anyhow::Error;

#[derive(Debug, Clone)]
pub struct StudioError {
    pub code: &'static str,
    pub message: String,
    pub pointer: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub current_revision: Option<u64>,
    pub retryable: bool,
}

impl StudioError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            pointer: None,
            expected: None,
            actual: None,
            current_revision: None,
            retryable: false,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("VALIDATION_FAILED", message)
    }

    pub fn revision_conflict(expected: u64, current: u64) -> Self {
        Self {
            code: "REVISION_CONFLICT",
            message: format!("Expected revision {expected}; current revision is {current}"),
            pointer: Some("/expectedRevision".to_owned()),
            expected: Some(expected.to_string()),
            actual: Some(current.to_string()),
            current_revision: Some(current),
            retryable: false,
        }
    }

    pub fn with_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.pointer = Some(pointer.into());
        self
    }

    pub fn with_expected(mut self, expected: impl Into<String>) -> Self {
        self.expected = Some(expected.into());
        self
    }

    pub fn with_actual(mut self, actual: impl Into<String>) -> Self {
        self.actual = Some(actual.into());
        self
    }
}

impl fmt::Display for StudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StudioError {}

pub fn find_studio_error(error: &Error) -> Option<&StudioError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<StudioError>())
}

pub fn legacy_error_code(error: &Error) -> Option<&'static str> {
    const CODES: &[&str] = &[
        "REVISION_CONFLICT",
        "IDEMPOTENCY_CONFLICT",
        "PROJECT_EXISTS",
        "PROJECT_NOT_FOUND",
        "REVISION_NOT_FOUND",
        "JOB_NOT_FOUND",
        "ASSET_NOT_FOUND",
        "UPLOAD_NOT_FOUND",
        "JOB_NOT_RETRYABLE",
        "JOB_RETRY_LIMIT",
    ];
    error.chain().find_map(|cause| {
        let message = cause.to_string();
        let (prefix, _) = message.split_once(':')?;
        CODES.iter().copied().find(|code| *code == prefix)
    })
}

pub fn render_error_code(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("font") {
        "FONT_REQUIRED"
    } else if lower.contains("cancel") {
        "JOB_CANCELLED"
    } else if lower.contains("time limit") || lower.contains("timeout") {
        "RENDER_TIMEOUT"
    } else if lower.contains("failed to start") || lower.contains("media tool") {
        "TOOL_UNAVAILABLE"
    } else if lower.contains("validation")
        || lower.contains("not supported")
        || lower.contains("unsupported")
    {
        "VALIDATION_FAILED"
    } else {
        "RENDER_FAILED"
    }
}
