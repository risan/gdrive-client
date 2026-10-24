use std::fmt;
use std::path::PathBuf;

use crate::credentials::{Coordinator, Credentials, Need, Refreshed, Token};
use crate::error::{Error, Operation};
use crate::http::{Answer, Call, Failure, Request, Retrying, Transport, form_body, url};
use crate::options::Options;
use crate::secret::Secret;
use crate::token_store::{self, StoredTokens};

/// A blocking Drive client. It owns the credentials, the current access token and the token store's path, and can be
/// shared between threads.
pub struct Client {
    store_path: PathBuf,
    options: Options,
    transport: Transport,
    tokens: Coordinator,
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

        Ok(Client::assemble(store_path, options, credentials, None))
    }

    /// Opens an existing token store.
    pub fn open(token_store: impl Into<PathBuf>, options: Options) -> Result<Client, Error> {
        options.validate()?;

        let store_path = token_store.into();
        let stored = token_store::load(&store_path)?;
        let access_token = stored.access_token.map(|token| (token, stored.access_token_refresh_at));

        Ok(Client::assemble(store_path, options, stored.credentials, access_token))
    }

    fn assemble(
        store_path: PathBuf,
        options: Options,
        credentials: Credentials,
        access_token: Option<(Secret, Option<u64>)>,
    ) -> Client {
        Client {
            store_path,
            transport: Transport::new(&options),
            tokens: Coordinator::new(credentials, access_token),
            options,
        }
    }

    /// The latest failure to write a refreshed access token to the token store, which clears it. Such a failure does
    /// not fail the call that refreshed the token: the new token is used from memory. With several threads, any of
    /// them may take it.
    pub fn take_store_failure(&self) -> Option<Error> {
        self.tokens.take_store_failure()
    }

    pub(crate) fn call(&self, operation: Operation, target: impl Into<String>) -> Call {
        let call = Call::new(operation, target, self.options.deadline);

        call.know_all(self.tokens.secrets());
        call
    }

    pub(crate) fn chunk_size(&self) -> usize {
        self.options.chunk_size
    }

    pub(crate) fn drive_endpoint(&self) -> &str {
        &self.options.endpoints.drive
    }

    pub(crate) fn transport(&self) -> &Transport {
        &self.transport
    }

    pub(crate) fn drive_url(&self, path: &str, query: &[(&str, &str)]) -> String {
        url(&self.options.endpoints.drive, path, query)
    }

    pub(crate) fn token(&self, call: &Call, need: Need) -> Result<Token, Error> {
        let token = self.tokens.token(
            call,
            need,
            &mut |credentials| self.refresh(call, credentials),
            &mut |snapshot| {
                token_store::save(
                    &self.store_path,
                    &StoredTokens {
                        credentials: snapshot.credentials.clone(),
                        access_token: snapshot.access_token.clone(),
                        access_token_refresh_at: snapshot.refresh_at_unix,
                    },
                )
            },
        )?;

        call.know(token.secret.clone());
        Ok(token)
    }

    fn refresh(&self, call: &Call, credentials: &Credentials) -> Result<Refreshed, Error> {
        // Another thread may have rotated the refresh token since this call started, so the call's own list can be stale.
        call.know_all([credentials.client_secret.clone(), credentials.refresh_token.clone()]);

        let refresh_call = call.derive(Operation::RefreshToken, "");
        let form = form_body(&[
            ("grant_type", "refresh_token"),
            ("client_id", &credentials.client_id),
            ("client_secret", credentials.client_secret.expose()),
            ("refresh_token", credentials.refresh_token.expose()),
        ]);
        let request = Request::oauth(url(&self.options.endpoints.oauth, "/token", &[]), &form);
        let answer = self.transport.execute(&refresh_call, &request)?;
        let body = refresh_call.expect_oauth_json(&answer)?;
        let access_token = body["access_token"]
            .as_str()
            .ok_or_else(|| refresh_call.protocol_error("the answer has no access_token"))?;
        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
        let rotated = body["refresh_token"]
            .as_str()
            .filter(|token| *token != credentials.refresh_token.expose())
            .map(Secret::new);

        call.know(Secret::new(access_token));
        call.know_all(rotated.clone());

        Ok(Refreshed {
            access_token: Secret::new(access_token),
            expires_in: std::time::Duration::from_secs(expires_in),
            rotated_refresh_token: rotated,
        })
    }

    /// Sends a request with a bearer token and returns the response unread. A 401 spends the one forced refresh and
    /// asks the caller to resend at once.
    pub(crate) fn send_authorized(
        &self,
        call: &Call,
        request: &Request<'_>,
        retrying: &mut Retrying,
    ) -> Result<ureq::http::Response<ureq::Body>, Failure> {
        let token = self.token(call, Need::Current).map_err(Failure::Fatal)?;
        let response = self.transport.send(call, request, Some(&token.secret))?;

        if response.status().as_u16() == 401 && !retrying.reauthenticated {
            retrying.reauthenticated = true;
            self.token(call, Need::AfterUnauthorized(token.generation))
                .map_err(Failure::Fatal)?;

            return Err(Failure::Reauthenticated);
        }

        Ok(response)
    }

    /// One attempt of an authorized request: send, read the body, judge the status.
    pub(crate) fn attempt(
        &self,
        call: &Call,
        request: &Request<'_>,
        retrying: &mut Retrying,
    ) -> Result<Answer, Failure> {
        let response = self.send_authorized(call, request, retrying)?;

        self.transport.answer(call, request, response)
    }

    /// Sends an authorized request, retrying transient failures, until an answer to act on or the deadline.
    pub(crate) fn execute(&self, call: &Call, request: &Request<'_>) -> Result<Answer, Error> {
        let mut retrying = Retrying::new();

        loop {
            match self.attempt(call, request, &mut retrying) {
                Ok(answer) => return Ok(answer),
                Err(Failure::Reauthenticated) => {}
                Err(Failure::Transient {
                    description,
                    retry_after,
                }) => retrying.pause(&self.transport, call, &description, retry_after)?,
                Err(Failure::Fatal(error)) => return Err(retrying.attribute(error)),
            }
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("client_id", &self.tokens.client_id())
            .field("token_store", &self.store_path)
            .finish_non_exhaustive()
    }
}
