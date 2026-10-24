use std::fmt;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::http::{Call, unix_now};
use crate::secret::Secret;

/// A new access token is fetched this long before Google says the old one expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);
/// How often a call re-checks a refresh or a store write that another call is doing.
const POLL: Duration = Duration::from_millis(2);

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

/// What the token endpoint answered to a refresh.
pub(crate) struct Refreshed {
    pub access_token: Secret,
    pub expires_in: Duration,
    /// Set when Google sent a refresh token other than the one used.
    pub rotated_refresh_token: Option<Secret>,
}

/// An access token and the generation (+1 per new token) it belongs to.
#[derive(Debug)]
pub(crate) struct Token {
    pub secret: Secret,
    pub generation: u64,
}

/// Why a caller asks the coordinator for a token.
#[derive(Clone, Copy)]
pub(crate) enum Need {
    /// Any token that is not about to expire.
    Current,
    /// A 401 answered a request made with this generation: a newer token will do, else refresh.
    AfterUnauthorized(u64),
}

/// What the token store must be given.
pub(crate) struct Snapshot {
    pub credentials: Credentials,
    pub access_token: Option<Secret>,
    pub refresh_at_unix: Option<u64>,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owed {
    Nothing,
    AccessToken,
    Credentials,
}

pub(crate) type Refresher<'a> = dyn FnMut(&Credentials) -> Result<Refreshed, Error> + 'a;
pub(crate) type Writer<'a> = dyn FnMut(&Snapshot) -> Result<(), Error> + 'a;

struct State {
    credentials: Credentials,
    access_token: Option<Secret>,
    refresh_at: Option<Instant>,
    refresh_at_unix: Option<u64>,
    generation: u64,
    refreshing: bool,
    owed: Owed,
    store_failure: Option<Error>,
}

/// The access token, its refresh and its persistence (D8, D9). Only one refresh is in flight at a time, because a
/// second one would send a refresh token the first may have just invalidated; the state lock is never held across
/// network or disk I/O, so no call waits past its own deadline.
pub(crate) struct Coordinator {
    state: Mutex<State>,
    write_gate: Mutex<()>,
}

/// Clears the `refreshing` flag on every exit path of a refresh that did not publish its answer.
struct RefreshGuard<'a> {
    coordinator: &'a Coordinator,
    armed: bool,
}

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.coordinator.lock().refreshing = false;
        }
    }
}

impl Coordinator {
    pub fn new(credentials: Credentials, stored_access_token: Option<(Secret, Option<u64>)>) -> Coordinator {
        let (access_token, refresh_at_unix) = match stored_access_token {
            Some((token, refresh_at_unix)) => (Some(token), refresh_at_unix),
            None => (None, None),
        };
        let now = Instant::now();
        // A moment too far away to represent leaves `refresh_at` empty, which refreshes on the first call.
        let refresh_at =
            refresh_at_unix.and_then(|unix| now.checked_add(Duration::from_secs(unix.saturating_sub(unix_now()))));

        Coordinator {
            state: Mutex::new(State {
                credentials,
                access_token,
                refresh_at,
                refresh_at_unix,
                generation: 0,
                refreshing: false,
                owed: Owed::Nothing,
                store_failure: None,
            }),
            write_gate: Mutex::new(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The latest failure to write a refreshed access token to the store, taken (cleared) by this call.
    pub fn take_store_failure(&self) -> Option<Error> {
        self.lock().store_failure.take()
    }

    pub fn client_id(&self) -> String {
        self.lock().credentials.client_id.clone()
    }

    /// The secrets a call must keep out of error text: the client secret, the refresh token, the access token.
    pub fn secrets(&self) -> Vec<Secret> {
        let state = self.lock();
        let mut secrets = vec![
            state.credentials.client_secret.clone(),
            state.credentials.refresh_token.clone(),
        ];

        secrets.extend(state.access_token.clone());
        secrets
    }

    /// A token for one request. Pays a debt of credentials to the store first, waits for or performs a refresh when
    /// the token is missing, about to expire, or rejected, and writes the store after a refresh.
    pub fn token(
        &self,
        call: &Call,
        need: Need,
        refresh: &mut Refresher<'_>,
        write: &mut Writer<'_>,
    ) -> Result<Token, Error> {
        loop {
            let mut state = self.lock();

            if state.owed == Owed::Credentials {
                drop(state);
                self.write_store(call, write)?;
                continue;
            }

            let due = match need {
                Need::Current => state.access_token.is_none() || state.refresh_at.is_none_or(|at| Instant::now() >= at),
                Need::AfterUnauthorized(seen) => state.access_token.is_none() || state.generation == seen,
            };

            if !due {
                let secret = state.access_token.clone().expect("a token that is not due exists");

                return Ok(Token {
                    secret,
                    generation: state.generation,
                });
            }

            if state.refreshing {
                drop(state);

                if call.expired() {
                    return Err(call.deadline_error("waiting for another token refresh"));
                }

                thread::sleep(POLL);
                continue;
            }

            state.refreshing = true;

            let credentials = state.credentials.clone();

            drop(state);

            let mut guard = RefreshGuard {
                coordinator: self,
                armed: true,
            };
            let refreshed = refresh(&credentials)?;
            let published = self.publish(refreshed, &mut guard);

            if let Err(error) = self.write_store(call, write) {
                let mut state = self.lock();

                if state.owed == Owed::Credentials {
                    return Err(error);
                }

                // A deadline that passes before an access-token write only skips it; the next refresh writes again.
                if !matches!(error, Error::Deadline { .. }) {
                    state.store_failure = Some(error);
                }
            }

            return Ok(published);
        }
    }

    /// Stores a refresh answer in memory at once, before any deadline check, so a rotated refresh token is never lost.
    fn publish(&self, refreshed: Refreshed, guard: &mut RefreshGuard<'_>) -> Token {
        let (refresh_at, refresh_at_unix) = refresh_moments(Instant::now(), unix_now(), refreshed.expires_in);
        let mut state = self.lock();

        state.generation += 1;
        state.access_token = Some(refreshed.access_token.clone());
        state.refresh_at = Some(refresh_at);
        state.refresh_at_unix = Some(refresh_at_unix);

        match refreshed.rotated_refresh_token {
            Some(rotated) if rotated != state.credentials.refresh_token => {
                state.credentials.refresh_token = rotated;
                state.owed = Owed::Credentials;
            }
            _ if state.owed == Owed::Nothing => state.owed = Owed::AccessToken,
            _ => {}
        }

        state.refreshing = false;
        guard.armed = false;

        Token {
            secret: refreshed.access_token,
            generation: state.generation,
        }
    }

    /// Writes the newest state through the write gate, which is waited for only until the call's deadline.
    fn write_store(&self, call: &Call, write: &mut Writer<'_>) -> Result<(), Error> {
        let _gate = loop {
            match self.write_gate.try_lock() {
                Ok(gate) => break gate,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    if call.expired() {
                        return Err(call.deadline_error("waiting to write the token store"));
                    }

                    thread::sleep(POLL);
                }
            }
        };

        if call.expired() {
            return Err(call.deadline_error("the deadline passed before the token store was written"));
        }

        let snapshot = {
            let state = self.lock();

            if state.owed == Owed::Nothing {
                return Ok(());
            }

            Snapshot {
                credentials: state.credentials.clone(),
                access_token: state.access_token.clone(),
                refresh_at_unix: state.refresh_at_unix,
                generation: state.generation,
            }
        };

        write(&snapshot)?;

        let mut state = self.lock();

        if state.generation == snapshot.generation {
            state.owed = Owed::Nothing;
        }

        Ok(())
    }

    #[cfg(test)]
    pub fn inspect(&self) -> Inspect {
        let state = self.lock();

        Inspect {
            refresh_token: state.credentials.refresh_token.expose().to_string(),
            owed: state.owed,
        }
    }
}

#[cfg(test)]
pub(crate) struct Inspect {
    pub refresh_token: String,
    pub owed: Owed,
}

/// When to refresh a token received now: `expires_in` minus the margin, as a monotonic moment and as unix seconds.
pub(crate) fn refresh_moments(received: Instant, received_unix: u64, expires_in: Duration) -> (Instant, u64) {
    let usable = expires_in.saturating_sub(REFRESH_MARGIN);

    // An `expires_in` too large to represent is not a real lifetime: treat the token as due at once.
    match received.checked_add(usable) {
        Some(at) => (at, received_unix.saturating_add(usable.as_secs())),
        None => (received, received_unix),
    }
}

#[cfg(test)]
mod tests;
