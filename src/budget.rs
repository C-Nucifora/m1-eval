// SPDX-License-Identifier: GPL-3.0-or-later
//! Run-wide limits for repeated expansion and parsing, not M1 language bounds.

use crate::EvalError;

pub(crate) const MAX_EXPANSIONS: u64 = 100_000;
pub(crate) const MAX_EXPANSION_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct ExpansionBudget {
    pub(crate) iterations_left: u64,
    pub(crate) bytes_left: usize,
}

impl Default for ExpansionBudget {
    fn default() -> Self {
        Self {
            iterations_left: MAX_EXPANSIONS,
            bytes_left: MAX_EXPANSION_BYTES,
        }
    }
}

impl ExpansionBudget {
    pub(crate) fn iterations(
        &mut self,
        count: u64,
        script: &str,
        at: usize,
    ) -> Result<(), EvalError> {
        self.iterations_left = self.iterations_left.checked_sub(count).ok_or_else(|| {
            EvalError::UnsupportedConstruct {
                kind: format!(
                    "expand iteration budget exhausted in {script} (limit {MAX_EXPANSIONS} per run)"
                ),
                at,
            }
        })?;
        Ok(())
    }

    pub(crate) fn source_bytes(
        &mut self,
        count: Option<usize>,
        script: &str,
        at: usize,
    ) -> Result<(), EvalError> {
        self.bytes_left = count.and_then(|count| self.bytes_left.checked_sub(count)).ok_or_else(|| {
            EvalError::UnsupportedConstruct {
                kind: format!("expand generated-source budget exhausted in {script} (limit {MAX_EXPANSION_BYTES} bytes per run)"),
                at,
            }
        })?;
        Ok(())
    }
}

/// Direct API calls own their budget; scheduled calls borrow the run's budget.
pub(crate) enum Budget<'a> {
    Owned(ExpansionBudget),
    Shared(&'a mut ExpansionBudget),
}

impl Default for Budget<'_> {
    fn default() -> Self {
        Self::Owned(ExpansionBudget::default())
    }
}

impl Budget<'_> {
    pub(crate) fn as_mut(&mut self) -> &mut ExpansionBudget {
        match self {
            Self::Owned(budget) => budget,
            Self::Shared(budget) => budget,
        }
    }
}
