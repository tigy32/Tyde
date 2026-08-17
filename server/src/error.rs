use anyhow::Error as AnyhowError;
use protocol::{CommandErrorCode, CommandErrorPayload, FrameKind, StreamPath};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    Internal,
    ProtocolViolation,
}

#[derive(Debug)]
pub(crate) struct AppError {
    pub kind: AppErrorKind,
    pub operation: &'static str,
    pub message: String,
    pub fatal: bool,
    pub source: Option<AnyhowError>,
}

impl AppError {
    pub fn invalid(operation: &'static str, message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::InvalidInput, operation, message, false)
    }

    pub fn invalid_with_source(
        operation: &'static str,
        message: impl Into<String>,
        source: impl Into<AnyhowError>,
    ) -> Self {
        Self::invalid(operation, message).with_source(source)
    }

    pub fn not_found(operation: &'static str, message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::NotFound, operation, message, false)
    }

    pub fn conflict(operation: &'static str, message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Conflict, operation, message, false)
    }

    pub fn internal(operation: &'static str, source: impl Into<AnyhowError>) -> Self {
        let source = source.into();
        Self {
            kind: AppErrorKind::Internal,
            operation,
            message: source.to_string(),
            fatal: false,
            source: Some(source),
        }
    }

    pub fn internal_message(
        operation: &'static str,
        message: impl Into<String>,
        source: impl Into<AnyhowError>,
    ) -> Self {
        Self::new(AppErrorKind::Internal, operation, message, false).with_source(source)
    }

    pub fn protocol(operation: &'static str, message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::ProtocolViolation, operation, message, false)
    }

    pub fn with_source(mut self, source: impl Into<AnyhowError>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn code(&self) -> CommandErrorCode {
        match self.kind {
            AppErrorKind::InvalidInput => CommandErrorCode::InvalidInput,
            AppErrorKind::NotFound => CommandErrorCode::NotFound,
            AppErrorKind::Conflict => CommandErrorCode::Conflict,
            AppErrorKind::Internal => CommandErrorCode::Internal,
            AppErrorKind::ProtocolViolation => CommandErrorCode::ProtocolViolation,
        }
    }

    pub fn to_payload(
        &self,
        request_stream: StreamPath,
        request_kind: FrameKind,
    ) -> CommandErrorPayload {
        CommandErrorPayload {
            stream: request_stream,
            request_kind,
            operation: self.operation.to_owned(),
            code: self.code(),
            message: self.message.clone(),
            fatal: self.fatal,
        }
    }

    fn new(
        kind: AppErrorKind,
        operation: &'static str,
        message: impl Into<String>,
        fatal: bool,
    ) -> Self {
        Self {
            kind,
            operation,
            message: message.into(),
            fatal,
            source: None,
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.operation, self.message)
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}
