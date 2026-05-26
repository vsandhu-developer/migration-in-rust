use std::time::Duration;

use anyhow::Result;
use tracing::warn;

use super::ErrorClassifier;

/// Adaptive backoff: 1000 * 1.5^(attempt-1) ms (1000, 1500, 2250 ms ...).
fn backoff_ms(attempt: u32) -> u64 {
    let multiplier = 1.5f64.powi((attempt as i32).saturating_sub(1));
    (1000.0 * multiplier).round() as u64
}

/// Retry a side-effecting async fn up to `max_attempts` times.
/// Re-throws immediately if the error is classified as non-retryable.
pub async fn retry<F, Fut>(mut op: F, max_attempts: u32, name: &str) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        match op().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let err_type = ErrorClassifier::classify_anyhow(&e);
                if !ErrorClassifier::is_retryable(err_type) {
                    return Err(e);
                }
                if attempt < max_attempts {
                    let ms = backoff_ms(attempt);
                    warn!(
                        op = name, attempt, max_attempts, backoff_ms = ms,
                        error = %e, "retry"
                    );
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry exhausted: {name}")))
}

/// Same as `retry` but returns the operation's value.
pub async fn retry_with_result<F, Fut, T>(
    mut op: F,
    max_attempts: u32,
    name: &str,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let err_type = ErrorClassifier::classify_anyhow(&e);
                if !ErrorClassifier::is_retryable(err_type) {
                    return Err(e);
                }
                if attempt < max_attempts {
                    let ms = backoff_ms(attempt);
                    warn!(
                        op = name, attempt, max_attempts, backoff_ms = ms,
                        error = %e, "retry"
                    );
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry exhausted: {name}")))
}
