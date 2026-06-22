// SPDX-License-Identifier: GPL-3.0-or-later
//! m1-eval: a stepped evaluator for the MoTeC M1 scripting language.
pub mod error;
pub use error::EvalError;

// --- M0 dependency/API smoke (temporary; removed in M3 when the real loader lands).
// Proves m1-core and m1-typecheck resolve, link, and expose the API the plan builds on.
#[cfg(test)]
mod _smoke {
    #[test]
    fn m1_core_parse_links() {
        let cst = m1_core::parse("Demo = 1.0;\n");
        assert!(cst.root().children().len() >= 1);
    }
}
