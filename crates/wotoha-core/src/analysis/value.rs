//! Bounded scalar values used by the versioned analysis model.
//!
//! Model output is deliberately wrapped before it enters the domain.  This
//! keeps malformed NaN/infinite values and accidental out-of-range scores out
//! of cached analysis records.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! bounded_value {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
        pub struct $name(f32);

        impl $name {
            pub const ZERO: Self = Self(0.0);
            pub const ONE: Self = Self(1.0);

            /// Creates a value only when it is finite and in the unit interval.
            pub const fn new(value: f32) -> Option<Self> {
                if value.is_finite() && value >= 0.0 && value <= 1.0 {
                    Some(Self(value))
                } else {
                    None
                }
            }

            pub const fn try_new(value: f32) -> Option<Self> {
                Self::new(value)
            }

            /// Returns the wrapped scalar.
            pub const fn get(self) -> f32 {
                self.0
            }

            /// Explicitly clamps an arbitrary scalar into the unit interval.
            ///
            /// Non-finite input is mapped to zero.  Callers that need to
            /// reject malformed model output should use [`Self::new`].
            pub fn clamped(value: f32) -> Self {
                if value.is_nan() {
                    Self::ZERO
                } else if value.is_sign_negative() {
                    Self::ZERO
                } else if value.is_infinite() || value >= 1.0 {
                    Self::ONE
                } else {
                    Self(value)
                }
            }

            /// Alias for [`Self::clamped`], making the lossy operation
            /// explicit at call sites.
            pub fn clamp(value: f32) -> Self {
                Self::clamped(value)
            }

            /// Compatibility spelling for callers that prefer constructor
            /// terminology when accepting lossy model output.
            pub fn new_clamped(value: f32) -> Self {
                Self::clamped(value)
            }

            pub fn from_clamped(value: f32) -> Self {
                Self::clamped(value)
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0.0
            }

            pub const fn is_one(self) -> bool {
                self.0 == 1.0
            }

            pub const fn validate(&self) -> bool {
                self.0.is_finite() && self.0 >= 0.0 && self.0 <= 1.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::ZERO
            }
        }

        impl From<$name> for f32 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_f32(self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = f32::deserialize(deserializer)?;
                Self::new(value).ok_or_else(|| {
                    serde::de::Error::custom(concat!(
                        stringify!($name),
                        " must be finite and in [0, 1]"
                    ))
                })
            }
        }
    };
}

bounded_value!(
    UnitInterval,
    "A finite scalar in the closed interval [0, 1]."
);
bounded_value!(ModelScore, "A bounded score emitted by an analysis model.");
bounded_value!(Confidence, "Confidence in an analysis observation.");
bounded_value!(Support, "Observed support for an analysis hypothesis.");

impl From<Confidence> for UnitInterval {
    fn from(value: Confidence) -> Self {
        Self::new(value.get()).unwrap_or(Self::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_and_out_of_range_values() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01, 1.01] {
            assert!(UnitInterval::new(value).is_none());
            assert!(ModelScore::new(value).is_none());
            assert!(Confidence::new(value).is_none());
            assert!(Support::new(value).is_none());
        }
        assert_eq!(UnitInterval::new(0.0).unwrap(), UnitInterval::ZERO);
        assert_eq!(ModelScore::new(1.0).unwrap(), ModelScore::ONE);
        assert_eq!(Confidence::new(0.0).unwrap(), Confidence::ZERO);
        assert_eq!(Support::new(1.0).unwrap(), Support::ONE);
        assert_eq!(UnitInterval::new(0.25).unwrap().get(), 0.25);
    }

    #[test]
    fn clamp_is_explicit_and_non_finite_safe() {
        assert_eq!(Confidence::clamped(-1.0), Confidence::ZERO);
        assert_eq!(Confidence::clamped(4.0), Confidence::ONE);
        assert_eq!(Confidence::clamped(f32::NAN), Confidence::ZERO);
        assert_eq!(Support::clamp(f32::INFINITY), Support::ONE);
    }
}
