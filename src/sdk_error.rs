use std::{error::Error, fmt};

use openlark_core::error::CoreError;

/// English diagnostic labels around SDK errors, preserving server messages and sources.
#[derive(Debug)]
pub(crate) struct SdkError(pub CoreError);

impl fmt::Display for SdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            CoreError::Api(error) => write!(
                f,
                "API error {} {}: {}",
                error.raw_code, error.endpoint, error.message
            ),
            CoreError::Network(error) => {
                if let Some(source) = &error.source {
                    write!(f, "Network error: {source}")
                } else {
                    write!(f, "Network error: {}", error.message)
                }
            }
            CoreError::Authentication { message, .. } => {
                write!(f, "Authentication failed: {message}")
            }
            CoreError::Validation { field, message, .. } => {
                write!(f, "Validation error for {field}: {message}")
            }
            CoreError::Configuration { message, .. } => write!(f, "Configuration error: {message}"),
            CoreError::Serialization { message, .. } => write!(f, "Serialization error: {message}"),
            CoreError::Business { code, message, .. } => {
                write!(f, "Business error {code:?}: {message}")
            }
            CoreError::Timeout {
                operation,
                duration,
                ..
            } => write!(f, "Operation {operation:?} timed out after {duration:?}"),
            CoreError::RateLimit {
                limit,
                window,
                reset_after,
                ..
            } => write!(
                f,
                "Rate limit exceeded: {limit} requests per {window:?}; reset after {reset_after:?}"
            ),
            CoreError::ServiceUnavailable {
                service,
                retry_after,
                ..
            } => write!(
                f,
                "Service unavailable: {service}; retry after {retry_after:?}"
            ),
            CoreError::ResponseTooLarge { limit, actual, .. } => write!(
                f,
                "Response too large: {actual} bytes exceeds the {limit} byte limit"
            ),
            CoreError::Internal { code, message, .. } => {
                write!(f, "Internal SDK error {code:?}: {message}")
            }
            _ => write!(f, "SDK error: {:?}", self.0),
        }?;
        if let Some(request_id) = self.0.record().request_id {
            write!(f, " (request_id: {request_id})")?;
        }
        Ok(())
    }
}

impl Error for SdkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        let source = match &self.0 {
            CoreError::Api(error) => &error.source,
            CoreError::Network(error) => &error.source,
            CoreError::Serialization { source, .. } | CoreError::Internal { source, .. } => source,
            _ => return None,
        };
        source
            .as_deref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlark_core::error::ErrorContext;

    #[test]
    fn sdk_api_labels_are_english_and_keep_diagnostics() {
        let error = SdkError(
            CoreError::api(131006, "/example", "Permission denied", ErrorContext::new())
                .with_request_id("request-1"),
        );
        let text = error.to_string();
        assert!(text.starts_with("API error 131006 /example: Permission denied"));
        assert!(text.contains("request-1"));
    }

    #[test]
    fn network_errors_preserve_the_original_source_chain() {
        let error = SdkError(CoreError::network(
            std::io::Error::other("Connection refused"),
            ErrorContext::new(),
        ));
        assert_eq!(error.to_string(), "Network error: Connection refused");
        assert_eq!(error.source().unwrap().to_string(), "Connection refused");
    }
}
