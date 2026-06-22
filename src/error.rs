// SPDX-License-Identifier: GPL-3.0-or-later
//! Fail-loud error type. The evaluator never substitutes a guessed value.

#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    /// A builtin object/method we do not implement (Tier-3 or unknown).
    UnsupportedBuiltin { object: String, method: String },
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
    /// Wrong argument count/kind for a builtin call.
    BadCall { detail: String },
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnsupportedBuiltin { object, method } => {
                write!(f, "unsupported builtin: {object}.{method}")
            }
            EvalError::UnsupportedConstruct { kind, at } => {
                write!(f, "unsupported construct {kind} at byte {at}")
            }
            EvalError::UnresolvedSymbol { name } => write!(f, "unresolved symbol: {name}"),
            EvalError::MissingCalibration { path } => {
                write!(f, "missing calibration value: {path}")
            }
            EvalError::TypeError { detail } => write!(f, "type error: {detail}"),
            EvalError::MissingInput { channel } => write!(f, "missing scenario input: {channel}"),
            EvalError::BadCall { detail } => write!(f, "bad call: {detail}"),
        }
    }
}
impl std::error::Error for EvalError {}
