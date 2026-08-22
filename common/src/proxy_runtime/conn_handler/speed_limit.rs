//! Validated egress speed limit for proxy listeners.
//!
//! [`SpeedLimit`] wraps the raw `f64` bytes/s value that is handed to
//! [`async_speed_limit::Limiter`]. It can only be constructed through the
//! validating [`SpeedLimit::try_new`] (raw value) or [`SpeedLimit::from_config`]
//! (optional config value) constructors, which reject anything that is not
//! finite and strictly positive. This makes an invalid speed limit impossible
//! to express at the type level — the only way to obtain a [`SpeedLimit`] is to
//! pass validation.

use thiserror::Error;

/// Validated egress speed limit in bytes/s.
///
/// A `None` config value means *unlimited*, represented internally as
/// [`f64::INFINITY`] — which [`async_speed_limit::Limiter`] treats as "no
/// limit". A `Some(value)` config value must be finite and strictly positive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeedLimit(f64);

impl SpeedLimit {
    /// Unlimited speed limit (internally [`f64::INFINITY`]).
    pub const UNLIMITED: SpeedLimit = SpeedLimit(f64::INFINITY);

    /// Build a validated speed limit from a raw bytes/s value.
    ///
    /// Returns [`SpeedLimitError`] if `value` is `NaN`, infinite, zero, or
    /// negative — i.e. anything that is not finite and strictly positive.
    pub fn try_new(value: f64) -> Result<SpeedLimit, SpeedLimitError> {
        if value.is_finite() && value > 0.0 {
            Ok(SpeedLimit(value))
        } else {
            Err(SpeedLimitError { value })
        }
    }

    /// Build a speed limit from an optional config value.
    ///
    /// `None` means unlimited. `Some(value)` must be finite and positive.
    pub fn from_config(value: Option<f64>) -> Result<SpeedLimit, SpeedLimitError> {
        match value {
            None => Ok(SpeedLimit::UNLIMITED),
            Some(value) => SpeedLimit::try_new(value),
        }
    }

    /// The raw bytes/s value to pass to [`async_speed_limit::Limiter::new`].
    pub fn into_inner(self) -> f64 {
        self.0
    }
}

impl Default for SpeedLimit {
    fn default() -> Self {
        SpeedLimit::UNLIMITED
    }
}

/// Error returned when a [`SpeedLimit`] cannot be constructed because the
/// supplied value is not finite and strictly positive.
#[derive(Debug, Error)]
#[error("must be finite and positive, got {value}")]
pub struct SpeedLimitError {
    pub value: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_from_none() {
        assert_eq!(
            SpeedLimit::from_config(None).unwrap(),
            SpeedLimit::UNLIMITED
        );
        assert_eq!(SpeedLimit::UNLIMITED.into_inner(), f64::INFINITY);
    }

    #[test]
    fn accepts_finite_positive() {
        assert!(SpeedLimit::try_new(1.0).is_ok());
        assert!(SpeedLimit::try_new(f64::MAX).is_ok());
        assert!(SpeedLimit::try_new(0.5).is_ok());
    }

    #[test]
    fn rejects_non_positive_and_non_finite() {
        assert!(SpeedLimit::try_new(0.0).is_err());
        assert!(SpeedLimit::try_new(-1.0).is_err());
        assert!(SpeedLimit::try_new(f64::INFINITY).is_err());
        assert!(SpeedLimit::try_new(f64::NEG_INFINITY).is_err());
        assert!(SpeedLimit::try_new(f64::NAN).is_err());
    }

    #[test]
    fn config_rejects_bad_values() {
        assert!(SpeedLimit::from_config(Some(0.0)).is_err());
        assert!(SpeedLimit::from_config(Some(f64::NAN)).is_err());
        assert!(SpeedLimit::from_config(Some(f64::INFINITY)).is_err());
    }
}
