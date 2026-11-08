use pretty_assertions::assert_eq;

use super::*;

#[test]
fn log_rate_trips_past_the_window_limit() {
    let mut rate = LogRate::default();
    assert!(!rate.add(LOG_BYTES_PER_WINDOW));
    assert!(rate.add(1));
}

#[test]
fn only_a_streak_of_fresh_misses_suspends() {
    let fresh = Duration::from_secs(10);
    let mut streak = CacheStreak::default();
    let verdicts = [
        streak.judge(400_000, fresh, 401_000, 0),
        streak.judge(401_000, fresh, 402_000, 0),
        streak.judge(402_000, fresh, 403_000, 401_408),
        streak.judge(403_000, fresh, 404_000, 0),
        streak.judge(404_000, CACHE_FRESH * 2, 405_000, 0),
        streak.judge(405_000, fresh, 406_000, 0),
        streak.judge(406_000, fresh, 407_000, 0),
    ];
    assert_eq!(
        verdicts.map(|verdict| verdict.is_some()),
        [false, false, false, false, false, false, true]
    );
}

#[test]
fn a_lockout_keeps_its_first_reason_across_restarts() {
    let home = tempfile::tempdir().expect("tempdir");
    let watchdog = Watchdog::load(home.path());
    watchdog.lock_out("first".to_string());
    watchdog.lock_out("second".to_string());
    let restarted = Watchdog::load(home.path());
    assert!(
        restarted
            .lockout()
            .is_some_and(|reason| reason.starts_with("first (at "))
    );
    std::fs::remove_file(restarted.path()).expect("audit lifts the lockout");
    assert_eq!(Watchdog::load(home.path()).lockout(), None);
}
