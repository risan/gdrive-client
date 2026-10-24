use std::fmt;
use std::path::PathBuf;

use crate::credentials::Credentials;
use crate::error::Error;
use crate::options::Options;
use crate::token_store::{self, StoredTokens};

/// A blocking Drive client. It owns the credentials, the current access token and the token store's path, and can be
/// shared between threads.
pub struct Client {
    store_path: PathBuf,
    options: Options,
    credentials: Credentials,
}

impl Client {
    /// Writes a new token store at `token_store` (replacing any old one) and makes no request.
    pub fn create(
        token_store: impl Into<PathBuf>,
        credentials: Credentials,
        options: Options,
    ) -> Result<Client, Error> {
        options.validate()?;

        let store_path = token_store.into();

        token_store::save(
            &store_path,
            &StoredTokens {
                credentials: credentials.clone(),
                access_token: None,
                access_token_refresh_at: None,
            },
        )?;

        Ok(Client {
            store_path,
            options,
            credentials,
        })
    }

    /// Opens an existing token store.
    pub fn open(token_store: impl Into<PathBuf>, options: Options) -> Result<Client, Error> {
        options.validate()?;

        let store_path = token_store.into();
        let stored = token_store::load(&store_path)?;

        Ok(Client {
            store_path,
            options,
            credentials: stored.credentials,
        })
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("client_id", &self.credentials.client_id)
            .field("token_store", &self.store_path)
            .finish_non_exhaustive()
    }
}
