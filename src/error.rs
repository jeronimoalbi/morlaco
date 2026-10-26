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
    /// A network can only be read with an API key the user has not entered.
    #[error(
        "{network} needs a {provider} API key. Add one under Data Providers… in the main menu."
    )]
    KeyNeeded {
        /// The network that cannot be read, for example "BNB Smart Chain".
        network: &'static str,
        /// Whose key is needed, for example "NodeReal".
        provider: &'static str,
    },
    /// A data provider refused the request and said why, for example that
    /// the API key is wrong.
    #[error("the data provider said: {0}")]
    Provider(String),
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
    /// A portfolio was given an empty name.
    #[error("a portfolio needs a name")]
    EmptyPortfolioName,
    /// Another portfolio already has this name.
    #[error("there is already a portfolio with this name")]
    PortfolioNameTaken,
    /// There is no portfolio with this id.
    #[error("there is no such portfolio")]
    NoSuchPortfolio,
    /// Removing the only portfolio would leave wallets with no home.
    #[error("the last portfolio cannot be removed")]
    LastPortfolio,
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
