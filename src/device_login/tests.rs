use super::*;

const SECOND: Duration = Duration::from_secs(1);

#[test]
fn poll_delay_is_the_greatest_of_interval_backoff_and_retry_after() {
    let interval = 5 * SECOND;

    assert_eq!(resend_delay(interval, SECOND, None), interval);
    assert_eq!(resend_delay(interval, 8 * SECOND, None), 8 * SECOND);
    assert_eq!(resend_delay(interval, SECOND, Some(20 * SECOND)), 20 * SECOND);
    assert_eq!(resend_delay(interval, 8 * SECOND, Some(2 * SECOND)), 8 * SECOND);
    assert_eq!(resend_delay(Duration::ZERO, Duration::ZERO, None), Duration::ZERO);
}

#[test]
fn slow_down_raises_every_later_wait_by_five_seconds() {
    let interval = SECOND;
    let slowed = slow_down(interval);
    let slowed_again = slow_down(slowed);

    assert_eq!(slowed, 6 * SECOND);
    assert_eq!(slowed_again, 11 * SECOND);
    assert_eq!(resend_delay(slowed, Duration::from_millis(50), None), 6 * SECOND);
}
