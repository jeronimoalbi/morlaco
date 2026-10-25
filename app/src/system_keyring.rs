//! The keyring of the operating system, where the database key is kept.
//!
//! Each platform has its own implementation of [`Keyring`], all named
//! `PlatformKeyring`, so the rest of the app does not care which one it uses.
//! A system with none gets a keyring that is never reachable, which makes the
//! app offer a passphrase instead.

use crate::keys::Keyring;

#[cfg(target_os = "linux")]
pub use secret_service::PlatformKeyring;

#[cfg(not(target_os = "linux"))]
pub use missing::PlatformKeyring;

/// The login keyring, through the Secret Service (libsecret).
#[cfg(target_os = "linux")]
mod secret_service {
    use std::collections::HashMap;

    use gtk4::gio;
    use libsecret::{
        COLLECTION_DEFAULT, Schema, SchemaAttributeType, SchemaFlags, password_clear_sync,
        password_lookup_sync, password_store_sync,
    };

    use super::Keyring;

    const SCHEMA_NAME: &str = "io.github.jeronimoalbi.Morlaco.DatabaseKey";
    const LABEL: &str = "Morlaco database key";

    /// The login keyring, through the Secret Service (libsecret).
    pub struct PlatformKeyring;

    fn schema() -> Schema {
        Schema::new(
            SCHEMA_NAME,
            SchemaFlags::NONE,
            HashMap::from([("purpose", SchemaAttributeType::String)]),
        )
    }

    fn attributes() -> HashMap<&'static str, &'static str> {
        HashMap::from([("purpose", "database-key")])
    }

    impl Keyring for PlatformKeyring {
        fn lookup(&self) -> Result<Option<String>, String> {
            password_lookup_sync(Some(&schema()), attributes(), gio::Cancellable::NONE)
                .map(|found| found.map(|s| s.to_string()))
                .map_err(|e| e.message().to_owned())
        }

        fn store(&self, secret: &str) -> Result<(), String> {
            password_store_sync(
                Some(&schema()),
                attributes(),
                Some(COLLECTION_DEFAULT),
                LABEL,
                secret,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.message().to_owned())
        }

        fn clear(&self) -> Result<(), String> {
            password_clear_sync(Some(&schema()), attributes(), gio::Cancellable::NONE)
                .map_err(|e| e.message().to_owned())
        }
    }
}

/// For systems without a supported keyring: it can never be reached.
#[cfg(not(target_os = "linux"))]
mod missing {
    use super::Keyring;

    const MESSAGE: &str = "this system has no supported keyring";

    /// A keyring that is never reachable.
    pub struct PlatformKeyring;

    impl Keyring for PlatformKeyring {
        fn lookup(&self) -> Result<Option<String>, String> {
            Err(MESSAGE.to_owned())
        }

        fn store(&self, _secret: &str) -> Result<(), String> {
            Err(MESSAGE.to_owned())
        }

        fn clear(&self) -> Result<(), String> {
            Err(MESSAGE.to_owned())
        }
    }
}
