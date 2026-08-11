use ash_core::ToolError;
use std::time::Duration;

pub fn parse_positive_seconds(seconds: f64) -> Result<Duration, ToolError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ToolError::Execution(
            "timeout must be a positive finite number of seconds".into(),
        ));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| ToolError::Execution("timeout is too large".into()))
}
