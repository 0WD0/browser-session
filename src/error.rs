// SPDX-License-Identifier: MIT

use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub auth_required: bool,
}

pub type Result<T> = std::result::Result<T, SessionError>;

impl SessionError {
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
        auth_required: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            auth_required,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid-request", message, false, false)
    }

    pub fn endpoint_in_use(message: impl Into<String>) -> Self {
        Self::new("endpoint-in-use", message, false, false)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new("cancelled", message, false, false)
    }
    pub fn auth_required(message: impl Into<String>) -> Self {
        Self::new("auth-required", message, false, true)
    }

    pub fn browser_restart_required(message: impl Into<String>) -> Self {
        Self::new("browser-restart-required", message, false, false)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new("network", message, true, false)
    }

    pub fn helper_failure(message: impl Into<String>) -> Self {
        Self::new("helper-failure", message, false, false)
    }
}

impl Display for SessionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionError {}

impl From<&str> for SessionError {
    fn from(message: &str) -> Self {
        Self::helper_failure(message)
    }
}

impl From<String> for SessionError {
    fn from(message: String) -> Self {
        Self::helper_failure(message)
    }
}
