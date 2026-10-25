//! The keyring of the operating system, where the database key is kept.
//!
//! Each platform has its own implementation of [`Keyring`], all named
//! `PlatformKeyring`, so the rest of the app does not care which one it uses.
//! A system with none gets a keyring that is never reachable, which makes the
//! app offer a passphrase instead.

use crate::keys::Keyring;

#[cfg(target_os = "linux")]
pub use secret_service::PlatformKeyring;

#[cfg(target_os = "macos")]
pub use keychain::PlatformKeyring;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

/// The login keychain of macOS.
#[cfg(target_os = "macos")]
mod keychain {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::Keyring;

    const SERVICE: &str = "io.github.jeronimoalbi.Morlaco";
    const ACCOUNT: &str = "database-key";
    /// The code the Keychain gives when there is no such item.
    const ITEM_NOT_FOUND: i32 = -25300;

    /// The login keychain, through Security.framework.
    pub struct PlatformKeyring;

    impl Keyring for PlatformKeyring {
        fn lookup(&self) -> Result<Option<String>, String> {
            match get_generic_password(SERVICE, ACCOUNT) {
                Ok(bytes) => String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| "the stored key is not text".to_owned()),
                Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }

        fn store(&self, secret: &str) -> Result<(), String> {
            set_generic_password(SERVICE, ACCOUNT, secret.as_bytes()).map_err(|e| e.to_string())
        }

        fn clear(&self) -> Result<(), String> {
            match delete_generic_password(SERVICE, ACCOUNT) {
                Err(e) if e.code() != ITEM_NOT_FOUND => Err(e.to_string()),
                _ => Ok(()),
            }
        }
    }
}

/// For systems without a supported keyring: it can never be reached.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
