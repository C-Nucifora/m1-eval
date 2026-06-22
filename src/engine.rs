// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`Engine`]: the public library facade over the loader, runners, and
//! coverage analysis.
//!
//! `Engine` is the one entry point a consumer (the visualiser, the CLI, a later
//! LSP) uses. It owns the loaded project internally and exposes only `m1-eval`'s
//! own types — [`Scenario`], [`Trace`], [`CoverageReport`], [`EvalError`]. No
//! `m1-core`/`m1-typecheck` type appears in any method signature, mirroring
//! `m1-doc`'s boundary discipline: there is exactly one engine, and the views
//! over it (visualiser, LSP) are thin.
//!
//! ```no_run
//! use m1_eval::{Engine, Scenario};
//! use std::path::Path;
//!
//! let engine = Engine::load(Path::new("Project.m1prj"), None)?;
//! let scenario = Scenario::from_toml_str("mode='function'\ntarget='F'\nduration_s=1.0\nbase_rate_hz=100.0")?;
//! let trace = engine.run(&scenario)?;
//! let coverage = engine.coverage();
//! # Ok::<(), m1_eval::EvalError>(())
//! ```

use crate::coverage::CoverageReport;
use crate::error::EvalError;
use crate::loader::{Loaded, load};
use crate::runner::run as run_scenario;
use crate::scenario::Scenario;
use crate::trace::Trace;
use std::path::Path;

/// A loaded M1 project ready to evaluate scenarios against.
///
/// Construct one with [`Engine::load`]; drive runs with [`Engine::run`]; inspect
/// what the engine can handle with [`Engine::coverage`]. The loaded project,
/// scripts, and calibration are private — the toolchain types never escape.
pub struct Engine {
    loaded: Loaded,
}

impl Engine {
    /// Load a project (and optional `.m1cfg` calibration) into an engine.
    ///
    /// `project` points at the `.m1prj`; scripts are discovered under its
    /// directory and calibration values read from `cfg` when given. Fails loud on
    /// a project that will not load or a calibration that will not parse.
    pub fn load(project: &Path, cfg: Option<&Path>) -> Result<Engine, EvalError> {
        let loaded = load(project, cfg)?;
        Ok(Engine { loaded })
    }

    /// Evaluate a scenario, producing a [`Trace`] of channel/expression values
    /// over the scenario's tick grid. Dispatches single-function or
    /// dependency-cone per the scenario's mode. Deterministic.
    pub fn run(&self, scenario: &Scenario) -> Result<Trace, EvalError> {
        run_scenario(&self.loaded, scenario)
    }

    /// Report which builtins/constructs every loaded script uses and whether the
    /// engine supports, stubs, or cannot handle each. Pure static analysis — no
    /// scenario needed; safe to call before [`Engine::run`].
    pub fn coverage(&self) -> CoverageReport {
        CoverageReport::analyse_in(&self.loaded.scripts, Some(&self.loaded.project))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;
    use std::path::Path;

    fn mini_engine() -> Engine {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        Engine::load(&dir.join("Project.m1prj"), Some(&dir.join("parameters.m1cfg")))
            .expect("mini fixture loads through the engine")
    }

    #[test]
    fn load_then_run_yields_expected_output_column() {
        let engine = mini_engine();
        let toml = r#"
mode = "function"
target = "Demo.Update"
duration_s = 0.03
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Demo.Speed"
const = 20.0

[[inputs]]
channel = "Root.Demo.Gain"
const = 2.5
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = engine.run(&scenario).expect("engine run succeeds");

        // 0.03s at 100Hz = 3 ticks; Output = 20 * 2.5 = 50 each.
        assert_eq!(trace.time.len(), 3);
        let out = trace
            .channels
            .get("Root.Demo.Output")
            .expect("Output column present");
        assert_eq!(out, &vec![Value::Float(50.0); 3]);
    }

    #[test]
    fn coverage_reports_without_a_run() {
        // The mini fixture's Demo.Update uses only an assignment + a local; nothing
        // unsupported. The report is available straight after load.
        let engine = mini_engine();
        let report = engine.coverage();
        // No unsupported items in the mini fixture.
        assert!(
            report.unsupported.is_empty(),
            "unexpected unsupported: {:?}",
            report.unsupported
        );
    }

    #[test]
    fn engine_run_signature_uses_only_crate_types() {
        // A compile-level assertion that `run` takes a `Scenario` and returns a
        // `Result<Trace, EvalError>` — all m1-eval types. (If a toolchain type
        // leaked into the signature this would not compile.)
        fn _accepts(engine: &Engine, sc: &Scenario) -> Result<Trace, EvalError> {
            engine.run(sc)
        }
    }
}
