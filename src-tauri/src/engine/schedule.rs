// SPDX-License-Identifier: GPL-3.0-or-later

use std::{future::Future, time::Duration as StdDuration};

use serde_json::Value;

use super::{
    CloudError, EngineError, SyncEngine, DEFAULT_SYNC_INTERVAL_SECONDS, SYNC_INTERVAL_SECONDS,
    SYNC_RETRY_DELAY,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryStep {
    Immediate,
    AfterDelay,
    Stop,
}

fn retry_step(failed_attempts: usize) -> RetryStep {
    match failed_attempts {
        1 => RetryStep::Immediate,
        2 | 3 => RetryStep::AfterDelay,
        _ => RetryStep::Stop,
    }
}

fn retryable(error: &EngineError) -> bool {
    !matches!(
        error,
        EngineError::Busy
            | EngineError::Cloud(
                CloudError::NotConfigured
                    | CloudError::OAuth
                    | CloudError::DeviceAuthorizationRejected
                    | CloudError::DeviceAuthorizationExchangeFailed
                    | CloudError::Revoked
                    | CloudError::ReauthRequired
                    | CloudError::AuthenticationRejected
            )
    )
}

async fn run_retry_policy<E, Attempt, AttemptFuture, IsRetryable, Wait, WaitFuture>(
    mut attempt: Attempt,
    is_retryable: IsRetryable,
    mut wait: Wait,
) -> Result<(), E>
where
    Attempt: FnMut() -> AttemptFuture,
    AttemptFuture: Future<Output = Result<(), E>>,
    IsRetryable: Fn(&E) -> bool,
    Wait: FnMut(StdDuration) -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    let mut failed_attempts = 0;
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) if !is_retryable(&error) => return Err(error),
            Err(error) => {
                failed_attempts += 1;
                match retry_step(failed_attempts) {
                    RetryStep::Immediate => {}
                    RetryStep::AfterDelay => wait(SYNC_RETRY_DELAY).await,
                    RetryStep::Stop => return Err(error),
                }
            }
        }
    }
}

impl SyncEngine {
    pub fn sync_interval_seconds(&self) -> u64 {
        self.store
            .setting("sync_interval_seconds")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| SYNC_INTERVAL_SECONDS.contains(value))
            .unwrap_or(DEFAULT_SYNC_INTERVAL_SECONDS)
    }

    pub fn set_sync_interval_seconds(&self, seconds: u64) -> Result<(), EngineError> {
        if !SYNC_INTERVAL_SECONDS.contains(&seconds) {
            return Err(CloudError::Rejected.into());
        }
        if self.sync_interval_seconds() == seconds {
            return Ok(());
        }
        self.store
            .set_setting("sync_interval_seconds", &seconds.to_string())?;
        self.schedule_changed.notify_one();
        Ok(())
    }

    pub fn apply_sync_interval_from_state(&self, value: &Value) -> Result<(), EngineError> {
        let seconds = value
            .get("source")
            .and_then(|source| {
                source
                    .get("sync_interval_seconds")
                    .or_else(|| source.get("syncIntervalSeconds"))
            })
            .and_then(Value::as_u64);
        if let Some(seconds) = seconds.filter(|value| SYNC_INTERVAL_SECONDS.contains(value)) {
            self.set_sync_interval_seconds(seconds)?;
        }
        Ok(())
    }

    pub fn apply_sync_interval_from_control_response(
        &self,
        value: &Value,
    ) -> Result<(), EngineError> {
        if let Some(seconds) = value
            .get("syncIntervalSeconds")
            .and_then(Value::as_u64)
            .filter(|seconds| SYNC_INTERVAL_SECONDS.contains(seconds))
        {
            self.set_sync_interval_seconds(seconds)?;
        }
        Ok(())
    }

    pub async fn wait_for_sync_interval(&self) {
        loop {
            let changed = self.schedule_changed.notified();
            tokio::pin!(changed);
            let seconds = self.sync_interval_seconds();
            tokio::select! {
                _ = tokio::time::sleep(StdDuration::from_secs(seconds)) => return,
                _ = &mut changed => {}
            }
        }
    }

    pub async fn sync_with_retries(&self) -> Result<(), EngineError> {
        run_retry_policy(|| self.sync_once(), retryable, tokio::time::sleep).await
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, future::ready, time::Duration};

    use serde_json::json;

    use super::{retry_step, retryable, run_retry_policy, RetryStep};
    use crate::cloud::CloudError;
    use crate::engine::EngineError;

    #[test]
    fn authentication_and_revocation_errors_are_not_retried() {
        for error in [
            EngineError::Cloud(CloudError::NotConfigured),
            EngineError::Cloud(CloudError::OAuth),
            EngineError::Cloud(CloudError::DeviceAuthorizationRejected),
            EngineError::Cloud(CloudError::DeviceAuthorizationExchangeFailed),
            EngineError::Cloud(CloudError::Revoked),
            EngineError::Cloud(CloudError::ReauthRequired),
            EngineError::Cloud(CloudError::AuthenticationRejected),
        ] {
            assert!(!retryable(&error));
        }
        assert!(retryable(&EngineError::Cloud(CloudError::Unreachable)));
    }

    #[test]
    fn control_poll_and_sync_state_accept_the_supported_schedule() {
        let (engine, _directory) = crate::engine::tests::test_engine();

        engine
            .apply_sync_interval_from_control_response(&json!({ "syncIntervalSeconds": 1200 }))
            .expect("apply interval from control response");
        assert_eq!(engine.sync_interval_seconds(), 1200);

        engine
            .apply_sync_interval_from_state(&json!({
                "source": { "sync_interval_seconds": 60 }
            }))
            .expect("apply interval from sync state");
        assert_eq!(engine.sync_interval_seconds(), 60);
    }

    #[test]
    fn automatic_retry_schedule_has_one_immediate_and_two_delayed_retries() {
        assert_eq!(retry_step(1), RetryStep::Immediate);
        assert_eq!(retry_step(2), RetryStep::AfterDelay);
        assert_eq!(retry_step(3), RetryStep::AfterDelay);
        assert_eq!(retry_step(4), RetryStep::Stop);
    }

    #[tokio::test]
    async fn retry_policy_recovers_on_a_later_attempt_and_waits_twice() {
        let attempts = Cell::new(0);
        let mut waits = Vec::new();
        let result = run_retry_policy(
            || {
                let count = attempts.get() + 1;
                attempts.set(count);
                ready(if count < 4 { Err("temporary") } else { Ok(()) })
            },
            |_| true,
            |delay| {
                waits.push(delay);
                ready(())
            },
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(attempts.get(), 4);
        assert_eq!(waits, [Duration::from_secs(30), Duration::from_secs(30)]);
    }

    #[tokio::test]
    async fn retry_policy_stops_after_two_delayed_retries_or_a_terminal_failure() {
        let attempts = Cell::new(0);
        let mut waits = Vec::new();
        let result = run_retry_policy(
            || {
                attempts.set(attempts.get() + 1);
                ready(Err("temporary"))
            },
            |_| true,
            |delay| {
                waits.push(delay);
                ready(())
            },
        )
        .await;
        assert_eq!(result, Err("temporary"));
        assert_eq!(attempts.get(), 4);
        assert_eq!(waits.len(), 2);

        let terminal_attempts = Cell::new(0);
        let terminal_waits = Cell::new(0);
        let result = run_retry_policy(
            || {
                terminal_attempts.set(terminal_attempts.get() + 1);
                ready(Err("authentication"))
            },
            |error| *error != "authentication",
            |_| {
                terminal_waits.set(terminal_waits.get() + 1);
                ready(())
            },
        )
        .await;
        assert_eq!(result, Err("authentication"));
        assert_eq!(terminal_attempts.get(), 1);
        assert_eq!(terminal_waits.get(), 0);
    }
}
