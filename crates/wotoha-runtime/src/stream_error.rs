use std::{error::Error, fmt};

/// Typed status information that survives Songbird's lazy input boundary.
///
/// The runtime uses this only for retry classification; URLs and response
/// bodies are deliberately not stored in the error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HttpStreamError {
    status: u16,
}

impl HttpStreamError {
    pub(crate) const fn new(status: u16) -> Self {
        Self { status }
    }

    pub(crate) const fn status(self) -> u16 {
        self.status
    }
}

impl fmt::Display for HttpStreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HTTP stream rejected with status {}",
            self.status
        )
    }
}

impl Error for HttpStreamError {}
