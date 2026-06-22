// SPDX-License-Identifier: GPL-3.0-or-later
//! m1-eval: a stepped evaluator for the MoTeC M1 scripting language.
pub mod error;
pub use error::EvalError;

pub mod value;
pub use value::Value;

pub mod calib;
pub use calib::{CalTable, Calibration};
