use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use super::*;
use crate::error::{Operation, TokenStoreProblem};

fn credentials() -> Credentials {
    Credentials::new("id", "secret", "refresh-0")
}

fn call(budget_ms: u64) -> Call {
    Call::new(Operation::Metadata, "target", Duration::from_millis(budget_ms))
}

fn refreshed(token: &str) -> Refreshed {
    Refreshed {
        access_token: Secret::new(token),
        expires_in: Duration::from_secs(3600),
        rotated_refresh_token: None,
    }
}

fn rotated(token: &str, refresh_token: &str) -> Refreshed {
    Refreshed {
        rotated_refresh_token: Some(Secret::new(refresh_token)),
        ..refreshed(token)
    }
}

fn write_failure() -> Error {
    Error::TokenStore {
        path: "store".into(),
        problem: TokenStoreProblem::WriteFailed(io::Error::other("disk full")),
    }
}

fn is_write_failed(error: &Error) -> bool {
    matches!(
        error,
        Error::TokenStore {
            problem: TokenStoreProblem::WriteFailed(_),
            ..
        }
    )
}

fn is_deadline(error: &Error) -> bool {
    matches!(error, Error::Deadline { .. })
}

#[test]
fn refresh_at_is_receipt_plus_expires_in_minus_60() {
    let received = Instant::now();

    for (expires_in, expected_margin) in [(3600, 3540), (61, 1), (60, 0), (59, 0), (0, 0)] {
        let (at, unix) = refresh_moments(received, 1_000_000, Duration::from_secs(expires_in));

        assert_eq!(
            at - received,
            Duration::from_secs(expected_margin),
            "expires_in {expires_in}"
        );
        assert_eq!(unix, 1_000_000 + expected_margin, "expires_in {expires_in}");
    }
}

#[test]
fn an_expiry_too_far_to_represent_means_refresh_now() {
    let received = Instant::now();

    assert_eq!(
        refresh_moments(received, 1_000_000, Duration::from_secs(u64::MAX)),
        (received, 1_000_000)
    );

    let coordinator = Coordinator::new(credentials(), Some((Secret::new("stored"), Some(u64::MAX))));
    let token = coordinator
        .token(&call(1000), Need::Current, &mut |_| Ok(refreshed("fresh")), &mut |_| {
            Ok(())
        })
        .unwrap();

    assert_eq!(token.secret.expose(), "fresh");
}

#[test]
fn a_token_inside_the_margin_is_refreshed_on_every_call() {
    let coordinator = Coordinator::new(credentials(), None);
    let calls = AtomicUsize::new(0);
    let mut refresh = |_: &Credentials| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Refreshed {
            expires_in: Duration::from_secs(59),
            ..refreshed("short")
        })
    };

    for _ in 0..3 {
        coordinator
            .token(&call(1000), Need::Current, &mut refresh, &mut |_| Ok(()))
            .unwrap();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn a_401_on_an_old_generation_uses_the_current_token() {
    let coordinator = Coordinator::new(credentials(), None);
    let calls = AtomicUsize::new(0);
    let mut refresh = |_: &Credentials| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(refreshed("fresh"))
    };
    let first = coordinator
        .token(&call(1000), Need::Current, &mut refresh, &mut |_| Ok(()))
        .unwrap();
    let again = coordinator
        .token(
            &call(1000),
            Need::AfterUnauthorized(first.generation - 1),
            &mut refresh,
            &mut |_| Ok(()),
        )
        .unwrap();
    let forced = coordinator
        .token(
            &call(1000),
            Need::AfterUnauthorized(first.generation),
            &mut refresh,
            &mut |_| Ok(()),
        )
        .unwrap();

    assert_eq!(again.generation, first.generation);
    assert_eq!(forced.generation, first.generation + 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn only_one_refresh_in_flight() {
    let coordinator = Arc::new(Coordinator::new(credentials(), None));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let (started_sender, started) = mpsc::channel::<()>();
    let (release_sender, release) = mpsc::channel::<()>();

    let first = {
        let (coordinator, refreshes) = (Arc::clone(&coordinator), Arc::clone(&refreshes));

        thread::spawn(move || {
            let mut refresh = |_: &Credentials| {
                refreshes.fetch_add(1, Ordering::SeqCst);
                started_sender.send(()).unwrap();
                release.recv().unwrap();
                Ok(refreshed("from-first"))
            };

            coordinator.token(&call(5000), Need::Current, &mut refresh, &mut |_| Ok(()))
        })
    };

    started.recv().unwrap();

    let second = {
        let (coordinator, refreshes) = (Arc::clone(&coordinator), Arc::clone(&refreshes));

        thread::spawn(move || {
            let mut refresh = |_: &Credentials| {
                refreshes.fetch_add(1, Ordering::SeqCst);
                Ok(refreshed("from-second"))
            };

            coordinator.token(&call(5000), Need::Current, &mut refresh, &mut |_| Ok(()))
        })
    };

    thread::sleep(Duration::from_millis(100));

    // With a 200 ms deadline a third caller gives up while the first refresh is still blocked, and never refreshes.
    let started_waiting = Instant::now();
    let impatient = coordinator.token(
        &call(200),
        Need::Current,
        &mut |_: &Credentials| -> Result<Refreshed, Error> { panic!("a second refresh started") },
        &mut |_| Ok(()),
    );

    assert!(is_deadline(&impatient.unwrap_err()));
    assert!(started_waiting.elapsed() < Duration::from_millis(900));

    release_sender.send(()).unwrap();

    let first = first.join().unwrap().unwrap();
    let second = second.join().unwrap().unwrap();

    assert_eq!(first.secret.expose(), "from-first");
    assert_eq!(
        second.secret.expose(),
        "from-first",
        "the waiter uses the first caller's token"
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[test]
fn write_outcomes_follow_d8() {
    // A successful write clears what was owed.
    let coordinator = Coordinator::new(credentials(), None);
    let written = Mutex::new(Vec::new());

    coordinator
        .token(
            &call(1000),
            Need::Current,
            &mut |_| Ok(refreshed("a")),
            &mut |snapshot| {
                written.lock().unwrap().push(snapshot.generation);
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(*written.lock().unwrap(), [1]);
    assert_eq!(coordinator.inspect().owed, Owed::Nothing);
    assert!(coordinator.take_store_failure().is_none());

    // A failed write of an access token only records the failure and the call goes on.
    let coordinator = Coordinator::new(credentials(), None);
    let token = coordinator
        .token(&call(1000), Need::Current, &mut |_| Ok(refreshed("b")), &mut |_| {
            Err(write_failure())
        })
        .unwrap();

    assert_eq!(token.secret.expose(), "b");
    assert_eq!(coordinator.inspect().owed, Owed::AccessToken);
    assert!(is_write_failed(&coordinator.take_store_failure().unwrap()));
    assert!(coordinator.take_store_failure().is_none());

    // A failed write of rotated credentials fails the call, keeps them in memory and owed; the next call writes them
    // first and does not refresh again.
    let coordinator = Coordinator::new(credentials(), None);
    let refreshes = AtomicUsize::new(0);
    let mut refresh = |_: &Credentials| {
        refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(rotated("c", "refresh-1"))
    };
    let error = coordinator
        .token(&call(1000), Need::Current, &mut refresh, &mut |_| Err(write_failure()))
        .unwrap_err();

    assert!(is_write_failed(&error));
    assert_eq!(coordinator.inspect().refresh_token, "refresh-1");
    assert_eq!(coordinator.inspect().owed, Owed::Credentials);

    let stored_refresh_tokens = Mutex::new(Vec::new());
    let token = coordinator
        .token(&call(1000), Need::Current, &mut refresh, &mut |snapshot| {
            stored_refresh_tokens
                .lock()
                .unwrap()
                .push(snapshot.credentials.refresh_token.expose().to_string());
            Ok(())
        })
        .unwrap();

    assert_eq!(token.secret.expose(), "c");
    assert_eq!(*stored_refresh_tokens.lock().unwrap(), ["refresh-1"]);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(coordinator.inspect().owed, Owed::Nothing);
}

#[test]
fn a_call_owing_credentials_fails_until_the_write_succeeds() {
    let coordinator = Coordinator::new(credentials(), None);

    coordinator
        .token(
            &call(1000),
            Need::Current,
            &mut |_| Ok(rotated("c", "refresh-1")),
            &mut |_| Err(write_failure()),
        )
        .unwrap_err();

    for _ in 0..2 {
        let error = coordinator
            .token(
                &call(1000),
                Need::Current,
                &mut |_: &Credentials| -> Result<Refreshed, Error> { panic!("refreshed with a dead refresh token") },
                &mut |_| Err(write_failure()),
            )
            .unwrap_err();

        assert!(is_write_failed(&error));
    }
}

#[test]
fn rotated_credentials_survive_a_deadline() {
    // The refresher returns a rotated token just after the deadline has passed.
    let coordinator = Coordinator::new(credentials(), None);
    let writes = AtomicUsize::new(0);
    let error = coordinator
        .token(
            &call(50),
            Need::Current,
            &mut |_| {
                thread::sleep(Duration::from_millis(120));
                Ok(rotated("late", "refresh-late"))
            },
            &mut |_| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap_err();

    assert!(is_deadline(&error), "{error:?}");
    assert_eq!(writes.load(Ordering::SeqCst), 0, "no write after the deadline");
    assert_eq!(coordinator.inspect().refresh_token, "refresh-late");
    assert_eq!(coordinator.inspect().owed, Owed::Credentials);

    let written = Mutex::new(Vec::new());

    coordinator
        .token(
            &call(1000),
            Need::Current,
            &mut |_: &Credentials| -> Result<Refreshed, Error> { panic!("a valid token needs no refresh") },
            &mut |snapshot| {
                written
                    .lock()
                    .unwrap()
                    .push(snapshot.credentials.refresh_token.expose().to_string());
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(*written.lock().unwrap(), ["refresh-late"]);

    // The write gate is held by a stalled writer when the rotated token arrives.
    let coordinator = Arc::new(Coordinator::new(credentials(), None));
    let (stalled_sender, stalled) = mpsc::channel::<()>();
    let (release_sender, release) = mpsc::channel::<()>();
    let stalled_writer = {
        let coordinator = Arc::clone(&coordinator);

        thread::spawn(move || {
            coordinator.token(&call(5000), Need::Current, &mut |_| Ok(refreshed("first")), &mut |_| {
                stalled_sender.send(()).unwrap();
                release.recv().unwrap();
                Ok(())
            })
        })
    };

    stalled.recv().unwrap();

    let error = coordinator
        .token(
            &call(200),
            Need::AfterUnauthorized(1),
            &mut |_| Ok(rotated("second", "refresh-second")),
            &mut |_| Ok(()),
        )
        .unwrap_err();

    assert!(is_deadline(&error), "{error:?}");
    assert_eq!(coordinator.inspect().refresh_token, "refresh-second");
    assert_eq!(coordinator.inspect().owed, Owed::Credentials);

    release_sender.send(()).unwrap();
    stalled_writer.join().unwrap().unwrap();

    let written = Mutex::new(Vec::new());

    coordinator
        .token(
            &call(1000),
            Need::Current,
            &mut |_| Ok(refreshed("unused")),
            &mut |snapshot| {
                written
                    .lock()
                    .unwrap()
                    .push(snapshot.credentials.refresh_token.expose().to_string());
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(*written.lock().unwrap(), ["refresh-second"]);
}

#[test]
fn writes_take_the_latest_state() {
    let coordinator = Coordinator::new(credentials(), None);
    let generations = Mutex::new(Vec::new());
    let mut write = |snapshot: &Snapshot| {
        generations.lock().unwrap().push((
            snapshot.generation,
            snapshot.access_token.as_ref().unwrap().expose().to_string(),
        ));
        Ok(())
    };

    let first = coordinator
        .token(&call(1000), Need::Current, &mut |_| Ok(refreshed("one")), &mut write)
        .unwrap();

    coordinator
        .token(
            &call(1000),
            Need::AfterUnauthorized(first.generation),
            &mut |_| Ok(refreshed("two")),
            &mut write,
        )
        .unwrap();

    assert_eq!(
        *generations.lock().unwrap(),
        [(1, "one".to_string()), (2, "two".to_string())]
    );
}

#[test]
fn a_stalled_write_does_not_hold_another_call_past_its_deadline() {
    let coordinator = Arc::new(Coordinator::new(credentials(), None));
    let (stalled_sender, stalled) = mpsc::channel::<()>();
    let (release_sender, release) = mpsc::channel::<()>();
    let thread_a = {
        let coordinator = Arc::clone(&coordinator);

        thread::spawn(move || {
            coordinator.token(
                &call(5000),
                Need::Current,
                &mut |_| Ok(rotated("a", "refresh-a")),
                &mut |_| {
                    stalled_sender.send(()).unwrap();
                    release.recv().unwrap();
                    Ok(())
                },
            )
        })
    };

    stalled.recv().unwrap();

    // B owes credentials (thread A's rotated token) and waits for the gate with a 200 ms deadline.
    let started = Instant::now();
    let error = coordinator
        .token(&call(200), Need::Current, &mut |_| Ok(refreshed("b")), &mut |_| Ok(()))
        .unwrap_err();

    assert!(is_deadline(&error), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );

    release_sender.send(()).unwrap();
    thread_a.join().unwrap().unwrap();
}
