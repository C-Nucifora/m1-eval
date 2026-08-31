// SPDX-License-Identifier: GPL-3.0-or-later
//! Fail-loud error type. The evaluator never substitutes a guessed value.

#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    /// A builtin object/method we do not implement (Tier-3 or unknown).
    UnsupportedBuiltin { object: String, method: String },
    /// A known builtin whose captured signature is not enough to reproduce its
    /// runtime behavior. The reason names the missing contract or evidence so
    /// callers can distinguish this from an unknown builtin.
    UnsupportedBuiltinBehavior {
        object: String,
        method: String,
        reason: String,
    },
    /// A syntactic construct the evaluator does not handle.
    UnsupportedConstruct { kind: String, at: usize },
    /// An identifier that resolves to no project symbol / local / builtin.
    UnresolvedSymbol { name: String },
    /// A calibration value (parameter or table cell) the .m1cfg did not provide.
    MissingCalibration { path: String },
    /// A type mismatch surfaced at runtime (e.g. arithmetic on a String).
    TypeError { detail: String },
    /// An input the scenario was required to provide but did not.
    MissingInput { channel: String },
    /// Required hardware metadata had no exact/wildcard scenario value and the
    /// attached adapter declined the call.
    MissingHardwareMetadata { call: String },
    /// Wrong argument count/kind for a builtin call.
    BadCall { detail: String },
    /// An error wrapped with *where* it happened: the script whose execution
    /// failed and the tick instant (`None` for the once-only startup pass).
    /// Execution boundaries attach this so a fail-loud abort names the deepest
    /// failing script and time instead of surfacing bare.
    InScript {
        script: String,
        t: Option<f64>,
        source: Box<EvalError>,
    },
}

impl EvalError {
    /// Wrap this error with the script it escaped from and the tick instant it
    /// happened at (`None` = the startup pass). If a deeper execution boundary
    /// already attached context, preserve it so there is exactly one layer and
    /// it names the script where the error actually arose.
    pub(crate) fn in_script(self, script: &str, t: Option<f64>) -> EvalError {
        match self {
            already @ EvalError::InScript { .. } => already,
            source => EvalError::InScript {
                script: script.to_string(),
                t,
                source: Box::new(source),
            },
        }
    }

    /// The innermost error, looking through any [`EvalError::InScript`] context
    /// layer — match on this when deciding *what* went wrong rather than
    /// *where* it happened.
    pub fn root_cause(&self) -> &EvalError {
        match self {
            EvalError::InScript { source, .. } => source.root_cause(),
            other => other,
        }
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnsupportedBuiltin { object, method } => {
                write!(f, "unsupported builtin: {object}.{method}")
            }
            EvalError::UnsupportedBuiltinBehavior {
                object,
                method,
                reason,
            } => write!(
                f,
                "unsupported builtin behavior: {object}.{method}: {reason}"
            ),
            EvalError::UnsupportedConstruct { kind, at } => {
                write!(f, "unsupported construct {kind} at byte {at}")
            }
            EvalError::UnresolvedSymbol { name } => write!(f, "unresolved symbol: {name}"),
            EvalError::MissingCalibration { path } => {
                write!(f, "missing calibration value: {path}")
            }
            EvalError::TypeError { detail } => write!(f, "type error: {detail}"),
            EvalError::MissingInput { channel } => write!(f, "missing scenario input: {channel}"),
            EvalError::MissingHardwareMetadata { call } => write!(
                f,
                "missing required hardware metadata for {call}; add a scenario [[io]] value for this call or supply it from a HardwareAdapter"
            ),
            EvalError::BadCall { detail } => write!(f, "bad call: {detail}"),
            EvalError::InScript { script, t, source } => match t {
                Some(t) => write!(f, "in {script} at t = {t:.3} s: {source}"),
                None => write!(f, "in {script} at startup: {source}"),
            },
        }
    }
}
impl std::error::Error for EvalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EvalError::InScript { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_script_displays_tick_time_and_inner_error() {
        let err = EvalError::TypeError {
            detail: "division or modulo by zero".to_string(),
        }
        .in_script("ECU.Update.m1scr", Some(0.125));
        assert_eq!(
            err.to_string(),
            "in ECU.Update.m1scr at t = 0.125 s: type error: division or modulo by zero"
        );
    }

    #[test]
    fn root_cause_looks_through_the_context_layer() {
        let inner = EvalError::MissingInput {
            channel: "Root.Demo.Speed".to_string(),
        };
        let wrapped = inner.clone().in_script("Demo.Update.m1scr", Some(0.0));
        assert_eq!(wrapped.root_cause(), &inner);
        // An unwrapped error is its own root cause.
        assert_eq!(inner.root_cause(), &inner);
    }

    #[test]
    fn outer_boundary_preserves_deeper_script_context() {
        let inner = EvalError::TypeError {
            detail: "bad operand".to_string(),
        }
        .in_script("Helper.Compute.m1scr", Some(0.25));
        let outer = inner.clone().in_script("Caller.Update.m1scr", Some(0.25));
        assert_eq!(
            outer, inner,
            "the deepest failing script remains authoritative"
        );
    }

    #[test]
    fn in_script_displays_startup_phase_when_no_tick_is_open() {
        let err = EvalError::MissingInput {
            channel: "Root.Demo.Speed".to_string(),
        }
        .in_script("MR.Init.m1scr", None);
        assert_eq!(
            err.to_string(),
            "in MR.Init.m1scr at startup: missing scenario input: Root.Demo.Speed"
        );
    }

    #[test]
    fn known_missing_builtin_behavior_names_the_evidence_gap() {
        let err = EvalError::UnsupportedBuiltinBehavior {
            object: "MPSE".to_string(),
            method: "Solve".to_string(),
            reason: "the captured integration equation is unavailable".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "unsupported builtin behavior: MPSE.Solve: the captured integration equation is unavailable"
        );
    }
}
