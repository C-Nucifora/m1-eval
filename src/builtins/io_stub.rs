// SPDX-License-Identifier: GPL-3.0-or-later
//! Tier-3 IO builtins (`CanComms.*`, `Serial.*`, `System.*`, `Logging.*`) as
//! documented stubs. Filled in by M6 Task 19; until then every IO method fails
//! loud so nothing silently fabricates a hardware value.

use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::value::Value;

/// Evaluate one Tier-3 IO call. Placeholder pending Task 19: fail loud.
pub fn call(
    object: &str,
    method: &str,
    _args: &[Value],
    _ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    Err(EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    })
}
