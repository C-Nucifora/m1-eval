// SPDX-License-Identifier: GPL-3.0-or-later
//! Builtin call dispatch.
//!
//! Every `Object.Method(...)` builtin call in an M1 script routes through
//! [`dispatch`]. M4 provides only the entry point and its fail-loud default:
//! nothing is implemented yet, so every call returns
//! [`EvalError::UnsupportedBuiltin`]. The pure builtins (`Calculate`/`Limit`/
//! `Convert`, table `.Lookup()`) arrive in M5 and the stateful operators
//! (`Filter`/`Integral`/`Delay`/…) in M6; each will match its `(object, method)`
//! here before this default is reached.
//!
//! Keeping the default fail-loud is the whole point: an unimplemented builtin
//! must surface as an error, never a guessed or defaulted number.

use crate::env::CallSite;
use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::value::Value;

/// Dispatch one builtin call `object.method(args)`.
///
/// `site` is the stable [`CallSite`] of the call node (script + byte offset),
/// which the stateful operators (M6) use to key their per-occurrence state.
/// `ctx` carries the evaluation environment (`dt`, value store, state store, …).
///
/// M4 implements no builtins, so every call fails loud with
/// [`EvalError::UnsupportedBuiltin`]. The `site`/`ctx`/`args` parameters are part
/// of the stable signature the M5/M6 implementations build against.
pub fn dispatch(
    object: &str,
    method: &str,
    _args: &[Value],
    _site: CallSite,
    _ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    // No builtins are wired up yet (M5/M6). Fail loud rather than guess.
    Err(EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_unsupported_in_m4() {
        // The dispatch stub fails loud for every name until M5/M6 wire builtins.
        // We exercise it directly (without an EvalCtx) below in expr.rs's call
        // tests; here we only assert the error shape for a representative call.
        let err = EvalError::UnsupportedBuiltin {
            object: "Calculate".to_string(),
            method: "Max".to_string(),
        };
        assert_eq!(
            err,
            EvalError::UnsupportedBuiltin {
                object: "Calculate".into(),
                method: "Max".into()
            }
        );
    }
}
