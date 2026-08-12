//! Strongly-typed identifiers for the rows S1 persists.
//!
//! These are newtypes over `String`, not parsers. The master plan shows shapes
//! (`p-<short>`, `T-0012`, `r-0041`) but v1 defines no grammar for them, so the
//! only invariant enforced here is the one that is unambiguously a bug when
//! violated: an identifier is never empty or blank.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An identifier was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The identifier was empty or contained only whitespace.
    #[error("{type_name} must not be empty or blank")]
    Blank {
        /// The newtype that rejected the value.
        type_name: &'static str,
    },
}

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Construct, rejecting blank input.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value: String = value.into();
                if value.trim().is_empty() {
                    return Err(IdError::Blank {
                        type_name: stringify!($name),
                    });
                }
                Ok(Self(value))
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume into the underlying string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }
    };
}

id_newtype!(
    /// `project.id`.
    ProjectId
);
id_newtype!(
    /// `plan_version.id`.
    PlanVersionId
);
id_newtype!(
    /// `task.id`.
    TaskId
);
id_newtype!(
    /// `run.id`.
    RunId
);
id_newtype!(
    /// `attempt.id`.
    AttemptId
);
id_newtype!(
    /// `workspace.id`.
    WorkspaceId
);
id_newtype!(
    /// `policy_snapshot.hash`, which `run.policy_hash` references.
    PolicyHash
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        let id = RunId::new("r-0041").expect("valid");
        assert_eq!(id.as_str(), "r-0041");
        assert_eq!(id.to_string(), "r-0041");
        assert_eq!("r-0041".parse::<RunId>().expect("valid"), id);
    }

    #[test]
    fn rejects_blank() {
        assert_eq!(
            TaskId::new(""),
            Err(IdError::Blank {
                type_name: "TaskId"
            })
        );
        assert_eq!(
            TaskId::new("   \t "),
            Err(IdError::Blank {
                type_name: "TaskId"
            })
        );
    }

    #[test]
    fn serde_round_trip_validates() {
        let id = ProjectId::new("p-abc").expect("valid");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"p-abc\"");
        let back: ProjectId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);

        // Deserialization must not be a hole around the constructor.
        let err = serde_json::from_str::<ProjectId>("\"\"").unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn distinct_id_types_do_not_interconvert() {
        // Compile-time property, asserted by construction: these are different
        // types over the same representation.
        let run = RunId::new("x").expect("valid");
        let task = TaskId::new("x").expect("valid");
        assert_eq!(run.as_str(), task.as_str());
    }
}
