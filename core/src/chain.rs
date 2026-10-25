//! Chains, and what a chain reports about an address.
//!
//! A [`Chain`] says how to read and display one network's native coin. A
//! [`ChainBackend`] fetches an address's history and reports it as
//! [`Activity`] values, which is all the rest of the crate knows about a
//! chain.
//!
//! # Adding a chain
//!
//! 1. Add a [`Chain`] variant. The compiler then lists every `match` that
//!    needs an arm: key, name, symbol, decimals, address check, explorer URL
//!    and backend.
//! 2. Implement [`ChainBackend`] in `backend/`. Networks that work alike
//!    (Ethereum, BNB Smart Chain, Polygon, …) can share one backend that is
//!    configured per network.
//! 3. Teach a [`PriceSource`](crate::price::PriceSource) the new chain.
//!
//! Tokens are not supported yet: a wallet holds only its chain's native coin.

use crate::backend::bitcoin::BitcoinBackend;
use crate::backend::ethereum::EthereumBackend;
use crate::{Error, Result};

/// A blockchain network whose native coin can be tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Chain {
    /// Bitcoin.
    Bitcoin,
    /// Ethereum, with its native coin ether.
    Ethereum,
    /// A second chain with 18 decimals, so tests can cover several chains.
    #[cfg(test)]
    Fake,
}

impl Chain {
    /// Every chain the app can track.
    pub const ALL: &'static [Chain] = &[Chain::Bitcoin, Chain::Ethereum];

    /// Stable identifier used when saving data.
    pub fn key(self) -> &'static str {
        match self {
            Chain::Bitcoin => "bitcoin",
            Chain::Ethereum => "ethereum",
            #[cfg(test)]
            Chain::Fake => "fake",
        }
    }

    /// The chain with this [`key`](Self::key).
    pub fn from_key(key: &str) -> Option<Chain> {
        match key {
            "bitcoin" => Some(Chain::Bitcoin),
            "ethereum" => Some(Chain::Ethereum),
            #[cfg(test)]
            "fake" => Some(Chain::Fake),
            _ => None,
        }
    }

    /// Display name of the network.
    pub fn name(self) -> &'static str {
        match self {
            Chain::Bitcoin => "Bitcoin",
            Chain::Ethereum => "Ethereum",
            #[cfg(test)]
            Chain::Fake => "Fake Chain",
        }
    }

    /// Ticker of the native coin.
    pub fn symbol(self) -> &'static str {
        match self {
            Chain::Bitcoin => "BTC",
            Chain::Ethereum => "ETH",
            #[cfg(test)]
            Chain::Fake => "FAKE",
        }
    }

    /// Decimal places of the native coin: how many of its smallest units
    /// make one coin (10^decimals).
    pub fn decimals(self) -> u8 {
        match self {
            Chain::Bitcoin => 8,
            Chain::Ethereum => 18,
            #[cfg(test)]
            Chain::Fake => 18,
        }
    }

    /// Converts smallest units to whole coins. Precision beyond what a
    /// float holds is dropped, which is fine for display and valuation.
    pub fn to_coins(self, units: u128) -> f64 {
        units as f64 / 10f64.powi(i32::from(self.decimals()))
    }

    /// Checks that `address` is plausible before it is sent anywhere.
    ///
    /// This is a cheap shape check, not a checksum validation; a backend
    /// reports addresses the network rejects as [`Error::InvalidAddress`].
    pub fn check_address(self, address: &str) -> Result<()> {
        let plausible = match self {
            Chain::Bitcoin => {
                (14..=90).contains(&address.len())
                    && address.chars().all(|c| c.is_ascii_alphanumeric())
            }
            Chain::Ethereum => {
                address.len() == 42
                    && address.starts_with("0x")
                    && address[2..].chars().all(|c| c.is_ascii_hexdigit())
            }
            #[cfg(test)]
            Chain::Fake => !address.is_empty(),
        };
        if plausible {
            Ok(())
        } else {
            Err(Error::InvalidAddress)
        }
    }

    /// Link to a transaction in a block explorer.
    pub fn explorer_tx_url(self, txid: &str) -> String {
        match self {
            Chain::Bitcoin => format!("https://mempool.space/tx/{txid}"),
            Chain::Ethereum => format!("https://etherscan.io/tx/{txid}"),
            #[cfg(test)]
            Chain::Fake => format!("https://fake.example/tx/{txid}"),
        }
    }

    /// The code that reads this chain.
    pub fn backend(self) -> &'static dyn ChainBackend {
        match self {
            Chain::Bitcoin => &BitcoinBackend,
            Chain::Ethereum => &EthereumBackend,
            #[cfg(test)]
            Chain::Fake => &FakeBackend,
        }
    }
}

/// Identifies a tracked wallet: an address on a chain.
///
/// The same text can be a valid address on several chains (Ethereum and BNB
/// Smart Chain share one format), so the chain is part of the identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WalletId {
    /// The network.
    pub chain: Chain,
    /// The address on that network.
    pub address: String,
}

impl WalletId {
    /// Creates an id. Ethereum addresses are written in lowercase, so the same
    /// address typed in two ways is one wallet.
    pub fn new(chain: Chain, address: impl Into<String>) -> Self {
        let address = address.into();
        let address = match chain {
            Chain::Ethereum => address.to_ascii_lowercase(),
            _ => address,
        };
        Self { chain, address }
    }
}

/// What one confirmed transaction did to one wallet, in the chain's smallest
/// unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Transaction id.
    pub txid: String,
    /// Block height, used to order transactions.
    pub height: u64,
    /// Position inside the block, where the chain defines one. Orders
    /// transactions that share a block.
    pub index: Option<u32>,
    /// Block time as a Unix timestamp.
    pub time: i64,
    /// Coins received minus coins sent, **not** counting the fee. Negative
    /// when the wallet paid out.
    pub amount: i128,
    /// What the wallet paid in fees for this transaction, on top of
    /// `amount`. Zero for a wallet that funded nothing.
    pub fee: u128,
    /// Ids of the transactions whose outputs this one spends, on chains
    /// where that is meaningful. Lets transactions in one block be put in
    /// spending order.
    pub parents: Vec<String>,
}

/// Reads one chain's history.
pub trait ChainBackend: Sync {
    /// The chain this backend reads.
    fn chain(&self) -> Chain;

    /// Downloads the confirmed activity of `address` in blocks above
    /// `since_height`, oldest first. Pass 0 for the full history.
    ///
    /// `on_progress` is called while paging with the number of activities
    /// found so far.
    fn fetch(
        &self,
        address: &str,
        since_height: u64,
        on_progress: &mut dyn FnMut(usize),
    ) -> Result<Vec<Activity>>;
}

/// Backend of the test-only chain: it never has any history.
#[cfg(test)]
struct FakeBackend;

#[cfg(test)]
impl ChainBackend for FakeBackend {
    fn chain(&self) -> Chain {
        Chain::Fake
    }

    fn fetch(&self, _: &str, _: u64, _: &mut dyn FnMut(usize)) -> Result<Vec<Activity>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_for_every_chain() {
        for chain in [Chain::Bitcoin, Chain::Ethereum, Chain::Fake] {
            assert_eq!(Chain::from_key(chain.key()), Some(chain));
        }
        assert_eq!(Chain::from_key("dogecoin"), None);
    }

    #[test]
    fn every_listed_chain_has_a_matching_backend() {
        for chain in Chain::ALL {
            assert_eq!(chain.backend().chain(), *chain);
        }
    }

    #[test]
    fn units_convert_to_coins_by_decimals() {
        assert_eq!(Chain::Bitcoin.to_coins(150_000_000), 1.5);
        // 2.5 coins of an 18-decimals chain; larger than u64::MAX units.
        let units = 2_500_000_000_000_000_000_u128;
        assert!(units > u128::from(u64::MAX) / 10);
        assert_eq!(Chain::Fake.to_coins(units), 2.5);
    }

    #[test]
    fn implausible_bitcoin_addresses_are_rejected() {
        let btc = Chain::Bitcoin;
        assert!(btc.check_address("").is_err());
        assert!(btc.check_address("not an address!!!!!").is_err());
        assert!(
            btc.check_address("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq")
                .is_ok()
        );
    }

    #[test]
    fn explorer_links_use_the_chain_explorer() {
        assert_eq!(
            Chain::Bitcoin.explorer_tx_url("abc"),
            "https://mempool.space/tx/abc"
        );
    }

    #[test]
    fn the_same_address_on_two_chains_is_two_wallets() {
        assert_ne!(
            WalletId::new(Chain::Bitcoin, "x"),
            WalletId::new(Chain::Fake, "x")
        );
    }

    #[test]
    fn ethereum_addresses_need_0x_and_forty_hex_digits() {
        let eth = Chain::Ethereum;
        assert!(
            eth.check_address("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
                .is_ok()
        );
        assert!(eth.check_address("").is_err());
        assert!(
            eth.check_address("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
                .is_err()
        );
        assert!(
            eth.check_address("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA9604")
                .is_err()
        );
        assert!(
            eth.check_address("0xzzdA6BF26964aF9D7eEd9e03E53415D37aA96045")
                .is_err()
        );
        // A Bitcoin address is not an Ethereum address, and the other way.
        assert!(
            eth.check_address("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq")
                .is_err()
        );
    }

    #[test]
    fn ethereum_addresses_are_kept_in_lowercase() {
        let mixed = WalletId::new(
            Chain::Ethereum,
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
        );
        let lower = WalletId::new(
            Chain::Ethereum,
            "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
        );
        assert_eq!(mixed, lower);
        // Bitcoin addresses are case sensitive and stay as typed.
        assert_eq!(WalletId::new(Chain::Bitcoin, "AbC").address, "AbC");
    }
}
