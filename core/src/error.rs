/// Everything that can go wrong while building a portfolio.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The address was rejected locally or by the API.
    #[error("not a valid address for this network")]
    InvalidAddress,
    /// The API answered with HTTP 429.
    #[error("rate limited by the API, try again in a moment")]
    RateLimited,
    /// The request failed or the response could not be decoded.
    #[error("request failed: {0}")]
    Http(Box<ureq::Error>),
    /// A data provider answered with something this version cannot read.
    #[error("unexpected response from the data provider: {0}")]
    BadResponse(&'static str),
    /// The price API returned no price for the requested time.
    #[error("no price available for timestamp {0}")]
    NoPrice(i64),
    /// The local database failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// No price source covers this chain.
    #[error("no price source for {0}")]
    UnsupportedChain(&'static str),
    /// The database holds a chain this version does not know.
    #[error("the saved data mentions an unknown network: {0}")]
    UnknownChain(String),
    /// The database was written by version 1, which used a different layout.
    #[error("the saved data uses an older layout")]
    OldFormat,
    /// The database was written by a newer version of Morlaco.
    #[error("the saved data was written by a newer version of Morlaco (layout {0})")]
    NewerFormat(i64),
    /// The key does not open the database file.
    #[error("the database could not be unlocked with this key")]
    WrongKey,
    /// Creating the data directory or setting permissions failed.
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
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
