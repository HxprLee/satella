#[derive(Debug)]
pub enum AppError {
    NotAnError,
    /// The peer did not answer the handshake or the consent in time.
    TimedOut,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnError => write!(f, "not an error"),
            Self::TimedOut => write!(f, "the peer did not answer in time"),
        }
    }
}
