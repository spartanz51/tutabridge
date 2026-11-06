//! Pure decisions for worker outcomes, repair waits and retry cadence.
use std::time::Duration;

pub(super) const POLL_DELAY: Duration = Duration::from_secs(10);
pub(super) const LOG_INTERVAL: Duration = Duration::from_secs(300);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Configuration = 10,
    Session = 11,
    Storage = 12,
    Rejected = 13,
    Version = 14,
    Occupied = 15,
    Starting = 16,
    Network = 17,
    Failed = 18,
    Stopping = 19,
    Initialization = 20,
}
impl State {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Configuration => "Waiting for configuration; run tutabridge init (Docker: docker exec -it <container> init)",
            Self::Session => "Waiting for a saved session; run tutabridge init (Docker: docker exec -it <container> init)",
            Self::Storage => "Session storage unavailable; check volume permissions, GPG key and pass store",
            Self::Rejected => "Saved session rejected; run tutabridge init (existing entry preserved)",
            Self::Version => "Tuta rejects this version; update the image: docker compose pull && docker compose up -d",
            Self::Occupied => "Data volume occupied by another init or service",
            Self::Starting => "Starting IMAP and SMTP",
            Self::Network => "Tuta temporarily unavailable; retrying with backoff",
            Self::Failed => "Service failed; inspect startup diagnostics",
            Self::Stopping => "Stopping",
            Self::Initialization => "Initialization failed or cancelled; inspect diagnostics and saved session state",
        }
    }
    pub(super) fn from_code(code: i32) -> Self {
        match code {
            10 => Self::Configuration,
            11 => Self::Session,
            12 => Self::Storage,
            13 => Self::Rejected,
            14 => Self::Version,
            17 => Self::Network,
            20 => Self::Initialization,
            _ => Self::Failed,
        }
    }

    /// A rejected session waits for local repair; a rejected version requires
    /// a new process/image even if the saved session changes.
    pub(super) fn awaits_repair(self, previous: Option<&[u8]>, current: Option<&[u8]>) -> bool {
        self == Self::Version || (self == Self::Rejected && previous == current)
    }

    /// Return this wait and the backoff for the next attempt. Time is supplied
    /// by the supervisor; these rules need neither a clock nor a runtime.
    pub(super) fn retry_delay(self, backoff: Duration) -> (Duration, Duration) {
        if matches!(self, Self::Failed | Self::Network) {
            (backoff, backoff.saturating_mul(2).min(MAX_BACKOFF))
        } else {
            (POLL_DELAY, POLL_DELAY)
        }
    }

    pub(super) fn should_log(self, previous: Self, elapsed: Duration) -> bool {
        self != previous || elapsed >= LOG_INTERVAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_exit_codes_decode_without_accepting_supervisor_states() {
        for (code, expected) in [
            (10, State::Configuration),
            (11, State::Session),
            (12, State::Storage),
            (13, State::Rejected),
            (14, State::Version),
            (17, State::Network),
            (20, State::Initialization),
        ] {
            assert_eq!(State::from_code(code), expected);
        }
        for code in [-1, 0, 9, 15, 16, 18, 19, 21, i32::MAX] {
            assert_eq!(State::from_code(code), State::Failed);
        }
    }

    #[test]
    fn rejected_session_waits_only_until_the_local_fingerprint_changes() {
        let old = Some(b"old".as_slice());
        let new = Some(b"new".as_slice());
        assert!(State::Rejected.awaits_repair(old, old));
        assert!(State::Rejected.awaits_repair(None, None));
        assert!(!State::Rejected.awaits_repair(old, new));
        assert!(!State::Rejected.awaits_repair(None, new));
        assert!(!State::Rejected.awaits_repair(old, None));
    }

    #[test]
    fn version_rejection_requires_restart_regardless_of_local_repairs() {
        for previous in [None, Some(b"old".as_slice())] {
            for current in [None, Some(b"new".as_slice())] {
                assert!(State::Version.awaits_repair(previous, current));
                assert!(!State::Network.awaits_repair(previous, current));
                assert!(!State::Storage.awaits_repair(previous, current));
            }
        }
    }

    #[test]
    fn transient_failures_back_off_to_five_minutes() {
        for state in [State::Failed, State::Network] {
            let mut backoff = POLL_DELAY;
            for seconds in [10, 20, 40, 80, 160, 300, 300] {
                let (delay, next) = state.retry_delay(backoff);
                assert_eq!(delay, Duration::from_secs(seconds));
                backoff = next;
            }
        }
    }

    #[test]
    fn local_waits_reset_the_backoff_for_the_next_network_attempt() {
        for state in [
            State::Configuration,
            State::Session,
            State::Storage,
            State::Rejected,
            State::Version,
            State::Occupied,
            State::Starting,
            State::Stopping,
            State::Initialization,
        ] {
            let (delay, next) = state.retry_delay(MAX_BACKOFF);
            assert_eq!((delay, next), (POLL_DELAY, POLL_DELAY));
            assert_eq!(State::Network.retry_delay(next).0, POLL_DELAY);
        }
    }

    #[test]
    fn log_changes_immediately_and_repeat_only_at_the_reminder_boundary() {
        assert!(State::Session.should_log(State::Starting, Duration::ZERO));
        assert!(!State::Session.should_log(State::Session, Duration::ZERO));
        assert!(!State::Session.should_log(State::Session, LOG_INTERVAL - Duration::from_millis(1)));
        assert!(State::Session.should_log(State::Session, LOG_INTERVAL));
    }
}
