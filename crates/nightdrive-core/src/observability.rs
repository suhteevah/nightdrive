//! tracing-subscriber initialization for nightdrive crates.
use crate::NightdriveResult;

pub fn init() -> NightdriveResult<()> {
    use tracing_subscriber::{fmt, EnvFilter};
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .json()
        .try_init()
        .map_err(|e| crate::NightdriveError::Config(format!("tracing init: {e}")))?;
    Ok(())
}
