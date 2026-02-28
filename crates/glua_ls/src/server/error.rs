use std::error::Error;

/// Error type for server exit scenarios
pub struct ExitError(pub String);

impl std::fmt::Debug for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExitError: {}", self.0)
    }
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for ExitError {}
