use std::fmt;

use crate::secret::Secret;

/// A client id, a client secret and a refresh token: what the library needs to get access tokens.
#[derive(Clone)]
pub struct Credentials {
    pub(crate) client_id: String,
    pub(crate) client_secret: Secret,
    pub(crate) refresh_token: Secret,
}

impl Credentials {
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Credentials {
        Credentials {
            client_id: client_id.into(),
            client_secret: Secret::new(client_secret),
            refresh_token: Secret::new(refresh_token),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}
