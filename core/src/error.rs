/// Everything that can go wrong while building a portfolio.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The address was rejected locally or by the API.
    #[error("not a valid Bitcoin address")]
    InvalidAddress,
    /// The API answered with HTTP 429.
    #[error("rate limited by the API, try again in a moment")]
    RateLimited,
    /// The request failed or the response could not be decoded.
    #[error("request failed: {0}")]
    Http(Box<ureq::Error>),
    /// The price API returned no price for the requested time.
    #[error("no price available for timestamp {0}")]
    NoPrice(i64),
    /// The local cache failed.
    #[error("cache error: {0}")]
    Cache(#[from] rusqlite::Error),
}

impl From<ureq::Error> for Error {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::StatusCode(429) => Self::RateLimited,
            ureq::Error::StatusCode(400) => Self::InvalidAddress,
            other => Self::Http(Box::new(other)),
        }
    }
}

/// Result alias using the crate [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;
