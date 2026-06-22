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

use crate::counterfactual::Override;
use crate::coverage::CoverageReport;
use crate::diff::{Counterfactual, Diff};
use crate::error::EvalError;
use crate::loader::{Loaded, load};
use crate::log::Log;
use crate::runner::{CounterfactualCfg, run as run_scenario, run_counterfactual};
use crate::scenario::Scenario;
use crate::trace::Trace;
use std::path::Path;

/// A loaded M1 project ready to evaluate scenarios against.
///
/// Construct one with [`Engine::load`]; drive runs with [`Engine::run`]; inspect
/// what the engine can handle with [`Engine::coverage`]. The loaded project,
/// scripts, and calibration are private — the toolchain types never escape.
///
/// A counterfactual baseline log can be attached with [`Engine::load_log`]; it is
/// stored as `Option<Log>` (initially `None`) and consumed by a later
/// counterfactual run as ground truth.
pub struct Engine {
    loaded: Loaded,
    /// The counterfactual ground-truth log, once attached via [`Engine::load_log`].
    /// `None` until a log is loaded; a subsequent counterfactual run uses it as the
    /// baseline every logged channel is held at.
    log: Option<Log>,
    /// Accumulated channel overrides ([`Engine::override_channel`]), layered over
    /// the log in a [`Engine::run_counterfactual`]. Empty until the first override.
    overrides: Vec<Override>,
}

impl Engine {
    /// Load a project (and optional `.m1cfg` calibration) into an engine.
    ///
    /// `project` points at the `.m1prj`; scripts are discovered under its
    /// directory and calibration values read from `cfg` when given. Fails loud on
    /// a project that will not load or a calibration that will not parse. The
    /// counterfactual log starts unset (`log: None`).
    pub fn load(project: &Path, cfg: Option<&Path>) -> Result<Engine, EvalError> {
        let loaded = load(project, cfg)?;
        Ok(Engine {
            loaded,
            log: None,
            overrides: Vec::new(),
        })
    }

    /// Attach a recorded run as the counterfactual ground-truth baseline.
    ///
    /// Dispatches on the file extension (case-insensitive):
    /// - `.csv` → [`Log::from_csv`] (the always-available, unencumbered path);
    /// - `.ld`  → the clean-room binary reader, behind the `ld` cargo feature.
    ///   Built without that feature, an `.ld` path fails loud, naming the feature
    ///   to rebuild with — never a silent skip or a guessed value.
    ///
    /// CSV bytes are decoded lossily (Windows-1252 i2 exports do not abort the
    /// load); `.ld` is read as raw bytes and handed to the binary reader. The
    /// parsed [`Log`] is stored on the engine so a later counterfactual run uses it
    /// as the baseline. Any unknown extension fails loud.
    pub fn load_log(&mut self, path: &Path) -> Result<(), EvalError> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let source = path.display().to_string();
        let log = match ext.as_deref() {
            Some("csv") => {
                let bytes = std::fs::read(path).map_err(|e| EvalError::MissingInput {
                    channel: format!("{}: {e}", path.display()),
                })?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Log::from_csv(&text, source)?
            }
            Some("ld") => Self::load_ld(path, source)?,
            other => {
                let found = other.unwrap_or("(none)");
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!("log file extension `.{found}` (expected `.csv` or `.ld`)"),
                    at: 0,
                });
            }
        };
        self.log = Some(log);
        Ok(())
    }

    /// Read an `.ld` binary log into a [`Log`] when the `ld` feature is enabled.
    ///
    /// The clean-room binary reader (`Log::from_ld`, built on the MIT `motec-i2`
    /// crate) lands in Milestone P3-D. This milestone (P3-A.T3) only establishes
    /// the feature-gated dispatch: with the feature enabled the arm still fails
    /// loud — pointing at the P3-D reader — so the build stays green ahead of the
    /// reader. P3-D replaces this body with the real `Log::from_ld` decode.
    #[cfg(feature = "ld")]
    fn load_ld(_path: &Path, _source: String) -> Result<Log, EvalError> {
        Err(EvalError::UnsupportedConstruct {
            kind: "binary `.ld` import is not available yet (the clean-room \
                   reader lands in Milestone P3-D); use a `.csv` log for now"
                .to_string(),
            at: 0,
        })
    }

    /// Fail-loud `.ld` arm when the `ld` feature is *not* enabled.
    ///
    /// `.ld` is a binary format read by a clean-room, feature-gated reader. Without
    /// the feature we never guess: we surface a clear instruction to rebuild with
    /// `--features ld` rather than silently ignoring the log.
    #[cfg(not(feature = "ld"))]
    fn load_ld(_path: &Path, _source: String) -> Result<Log, EvalError> {
        Err(EvalError::UnsupportedConstruct {
            kind: "binary `.ld` log requires the `ld` feature; rebuild with \
                   --features ld (or supply a `.csv` log)"
                .to_string(),
            at: 0,
        })
    }

    /// The attached counterfactual baseline log, if one has been loaded via
    /// [`Engine::load_log`]. `None` until a log is attached.
    pub fn log(&self) -> Option<&Log> {
        self.log.as_ref()
    }

    /// Register a counterfactual channel override from a `CH=value-or-expression`
    /// spec (see [`Override::parse`]). Overrides accumulate; each one replaces a
    /// logged channel with a constant or an expression before the downstream cone
    /// recomputes. Call repeatedly to override several channels. Fails loud on a
    /// malformed spec (no `=`, empty channel, empty right-hand side).
    pub fn override_channel(&mut self, spec: &str) -> Result<(), EvalError> {
        let ov = Override::parse(spec)?;
        self.overrides.push(ov);
        Ok(())
    }

    /// Run the counterfactual replay: hold every logged channel at its logged value
    /// (ground truth), layer the accumulated [`Engine::override_channel`] overrides,
    /// and recompute only the downstream dependency cone of the overridden channels.
    /// Returns the resulting [`Trace`].
    ///
    /// Source precedence is calibration < log < override. Requires a log to have
    /// been attached with [`Engine::load_log`] first; without one there is no ground
    /// truth to replay, which fails loud. The base tick rate defaults to the
    /// project's fastest scheduled call rate (or 100 Hz when the project schedules
    /// nothing periodically); the duration defaults to the log's own duration.
    /// Deterministic: the same log and overrides always yield the same trace.
    ///
    /// (Milestone P3-C wraps this in a `Counterfactual { trace, diff }`; this
    /// milestone returns the bare [`Trace`].)
    pub fn run_counterfactual(&self) -> Result<Trace, EvalError> {
        let log = self.log.as_ref().ok_or_else(|| EvalError::MissingInput {
            channel: "counterfactual run needs a log: call load_log first".to_string(),
        })?;
        let cfg = CounterfactualCfg {
            base_rate_hz: self.default_counterfactual_rate(),
            // 0.0 = "auto" -> the runner uses the log's own duration.
            duration_s: 0.0,
        };
        run_counterfactual(&self.loaded, log, &self.overrides, &cfg)
    }

    /// Run the counterfactual replay and diff the result against the logged ground
    /// truth, returning a [`Counterfactual`] (the recomputed [`Trace`] plus the
    /// per-channel [`Diff`]). This is the headline Phase-3 output: "override this
    /// channel; here is the trace and exactly which downstream channels moved, and
    /// by how much." Requires a log (fails loud via [`Engine::run_counterfactual`]).
    pub fn run_counterfactual_diff(&self) -> Result<Counterfactual, EvalError> {
        let trace = self.run_counterfactual()?;
        // `run_counterfactual` has already established that a log is attached.
        let log = self
            .log
            .as_ref()
            .expect("run_counterfactual succeeded, so a log is attached");
        let diff = Diff::between(log, &trace);
        Ok(Counterfactual { trace, diff })
    }

    /// The default base tick rate for a counterfactual run: the project's fastest
    /// periodic call rate, or 100 Hz when no function schedules periodically (so a
    /// project of purely event/startup functions still grids at a sane rate). A
    /// counterfactual recomputes only the override cone, but the grid rate governs
    /// stateful-operator `dt`, so a project-derived default is the faithful choice.
    fn default_counterfactual_rate(&self) -> f64 {
        self.loaded
            .scripts
            .iter()
            .filter_map(|script| {
                let fn_symbol = self
                    .loaded
                    .project
                    .function_symbol_for_script(&script.name)?;
                self.loaded
                    .project
                    .symbols()
                    .get(&fn_symbol)
                    .and_then(|s| s.call_rate_hz)
            })
            .fold(None::<f64>, |acc, r| Some(acc.map_or(r, |m| m.max(r))))
            .unwrap_or(100.0)
    }

    /// Evaluate a scenario, producing a [`Trace`] of channel/expression values
    /// over the scenario's tick grid. Dispatches single-function, dependency-cone,
    /// or the whole-project multi-rate scheduler per the scenario's mode.
    /// Deterministic.
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
        Engine::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
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
    fn whole_project_run_through_engine_produces_every_scheduled_channel() {
        // Task 14: the whole-project multi-rate scheduler is reachable through the
        // unchanged `Engine::run` dispatch. The multirate fixture's fast (100 Hz)
        // channels update every tick; the slow (50 Hz) channels run on even ticks
        // and hold between. We seed `Slow Out` so the cross-rate Fast Writer read
        // on tick 0 succeeds, and observe `Slow Echo` (read by nothing) for the
        // pure zero-order-hold.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        let engine =
            Engine::load(&dir.join("Project.m1prj"), None).expect("multirate loads through engine");
        let toml = r#"
mode = "whole-project"
duration_s = 0.04
base_rate_hz = 100.0

[[inputs]]
channel = "Root.MR.Seed"
const = 3.0

[[inputs]]
channel = "Root.MR.Slow Out"
const = 6.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = engine
            .run(&scenario)
            .expect("whole-project engine run succeeds");

        // 0.04 s at 100 Hz = 4 ticks; every scheduled channel has a dense column.
        assert_eq!(trace.time.len(), 4);
        let fast = trace
            .channels
            .get("Root.MR.Fast Out")
            .expect("Fast Out column");
        assert_eq!(fast.len(), 4, "fast channel present every tick");
        // Slow Echo = Seed*2 = 6 on every even tick; held between -> all 6.
        let echo = trace
            .channels
            .get("Root.MR.Slow Echo")
            .expect("Slow Echo column");
        assert_eq!(echo, &vec![Value::Float(6.0); 4]);
        // The On-Startup function never runs in whole-project mode.
        assert!(!trace.channels.contains_key("Root.MR.Started"));
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

    /// Write `contents` to a uniquely-named file with `ext` under a fresh temp dir
    /// and return both (the dir must outlive the path, so it is returned too).
    fn temp_log(ext: &str, contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(format!("run.{ext}"));
        std::fs::write(&path, contents).expect("write temp log");
        (dir, path)
    }

    #[test]
    fn load_log_starts_none() {
        // A freshly loaded engine has no counterfactual baseline attached.
        let engine = mini_engine();
        assert!(engine.log().is_none(), "log must be None until load_log");
    }

    #[test]
    fn load_log_csv_attaches_channels_as_ground_truth() {
        // load_log dispatches a `.csv` to Log::from_csv and stores it; the getter
        // then sees the logged channels (the future counterfactual baseline).
        let csv = "time,Engine Speed,Wheel Speed\n\
                   s,rpm,km/h\n\
                   0.0,800,0\n\
                   0.5,1200,30\n";
        let (_dir, path) = temp_log("csv", csv);

        let mut engine = mini_engine();
        engine.load_log(&path).expect("CSV log attaches");

        let log = engine.log().expect("log attached after load_log");
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Engine Speed", "Wheel Speed"]);
        // The units row rode along into the log's provenance metadata.
        assert_eq!(
            log.meta.units.get("Engine Speed").map(String::as_str),
            Some("rpm")
        );
        // Source records the loaded path's provenance.
        assert!(
            log.meta.source.ends_with("run.csv"),
            "source = {}",
            log.meta.source
        );
    }

    #[test]
    fn load_log_csv_extension_is_case_insensitive() {
        // An uppercase `.CSV` extension still routes to the CSV reader.
        let csv = "time,Engine Speed\n0.0,800\n0.5,1200\n";
        let (_dir, path) = temp_log("CSV", csv);

        let mut engine = mini_engine();
        engine.load_log(&path).expect("uppercase .CSV log attaches");
        assert_eq!(engine.log().expect("attached").channels.len(), 1);
    }

    #[test]
    fn load_log_malformed_csv_fails_loud() {
        // A CSV whose first column is not `time` fails loud through load_log (the
        // Log::from_csv error propagates — no silently-empty log).
        let csv = "t,Engine Speed\n0.0,800\n";
        let (_dir, path) = temp_log("csv", csv);

        let mut engine = mini_engine();
        match engine.load_log(&path) {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected fail-loud on malformed CSV, got {other:?}"),
        }
        // A failed load leaves the engine without a (partial/garbage) log.
        assert!(engine.log().is_none(), "failed load must not attach a log");
    }

    #[test]
    fn load_log_missing_file_fails_loud() {
        // A `.csv` path that does not exist fails loud rather than panicking.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("does-not-exist.csv");
        let mut engine = mini_engine();
        match engine.load_log(&path) {
            Err(EvalError::MissingInput { .. }) => {}
            other => panic!("expected MissingInput for absent file, got {other:?}"),
        }
    }

    #[test]
    fn load_log_unknown_extension_fails_loud() {
        // An extension that is neither `.csv` nor `.ld` fails loud.
        let (_dir, path) = temp_log("txt", "time,x\n0,1\n");
        let mut engine = mini_engine();
        match engine.load_log(&path) {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(kind.contains(".txt"), "kind names the bad ext: {kind}");
            }
            other => panic!("expected UnsupportedConstruct for `.txt`, got {other:?}"),
        }
    }

    // The `.ld` arm is behind the `ld` feature. Built WITHOUT it, an `.ld` path
    // must fail loud naming the feature to rebuild with — never a silent skip.
    #[cfg(not(feature = "ld"))]
    #[test]
    fn load_log_ld_without_feature_fails_loud_naming_feature() {
        let (_dir, path) = temp_log("ld", "not really an ld file");
        let mut engine = mini_engine();
        match engine.load_log(&path) {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(
                    kind.contains("ld") && kind.contains("--features"),
                    "fail-loud message must name the `ld` feature: {kind}"
                );
            }
            other => panic!("expected fail-loud `.ld`-without-feature error, got {other:?}"),
        }
        assert!(engine.log().is_none());
    }

    // ---- P3-B Task 6: counterfactual orchestration through the engine ----

    /// An engine over the counterfactual fixture (Sensor → A → Mid → B → Result,
    /// plus the unrelated C → Other).
    fn cf_engine() -> Engine {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/counterfactual");
        Engine::load(&dir.join("Project.m1prj"), None).expect("counterfactual fixture loads")
    }

    /// A synthetic, mutually-consistent counterfactual log CSV: `Mid = Sensor*2`,
    /// `Result = Mid+1`, `Other = 42`, with Sensor ramping 10 → 20 → 30.
    const CF_LOG_CSV: &str = "time,Root.CF.Sensor,Root.CF.Mid,Root.CF.Result,Root.CF.Other\n\
                              0.00,10,20,21,42\n\
                              0.01,20,40,41,42\n\
                              0.02,30,60,61,42\n";

    #[test]
    fn override_channel_accumulates_and_runs_the_cone() {
        // Attach the log, override Sensor to 100, and run the counterfactual through
        // the engine. The override's cone [A, B] recomputes Mid (= 200) and Result
        // (= 201); the unrelated Other holds its logged value 42.
        let mut engine = cf_engine();
        let (_dir, path) = temp_log("csv", CF_LOG_CSV);
        engine.load_log(&path).expect("log attaches");
        engine
            .override_channel("Root.CF.Sensor=100.0")
            .expect("override parses");

        let trace = engine.run_counterfactual().expect("counterfactual runs");
        // Default duration = log duration (0.02 s) at the fallback 100 Hz base =
        // ticks at t = 0.00, 0.01 (the half-open [0, 0.02) interval) = 2 ticks.
        assert_eq!(trace.time.len(), 2);
        let mid = trace.channels.get("Root.CF.Mid").expect("Mid column");
        let result = trace.channels.get("Root.CF.Result").expect("Result column");
        let other = trace.channels.get("Root.CF.Other").expect("Other column");
        assert!(mid.iter().all(|v| *v == Value::Float(200.0)), "{mid:?}");
        assert!(
            result.iter().all(|v| *v == Value::Float(201.0)),
            "{result:?}"
        );
        // Other is unrelated to the override: it passes through at its logged value.
        assert!(other.iter().all(|v| *v == Value::Float(42.0)), "{other:?}");
    }

    #[test]
    fn run_counterfactual_without_a_log_fails_loud() {
        // No log attached: there is no ground truth to replay against — fail loud
        // rather than silently producing an empty or guessed trace.
        let engine = cf_engine();
        match engine.run_counterfactual() {
            Err(EvalError::MissingInput { .. }) => {}
            other => panic!("expected MissingInput without a log, got {other:?}"),
        }
    }

    #[test]
    fn override_channel_malformed_spec_fails_loud() {
        // A spec with no `=` is a malformed override — fail loud, accumulate nothing.
        let mut engine = cf_engine();
        match engine.override_channel("no-equals-here") {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct for a malformed spec, got {other:?}"),
        }
    }

    #[test]
    fn run_counterfactual_signature_uses_only_crate_types() {
        // A compile-level assertion that the counterfactual surface takes/returns
        // only m1-eval types — a toolchain type leaking in would fail to compile.
        fn _accepts(engine: &Engine) -> Result<Trace, EvalError> {
            engine.run_counterfactual()
        }
    }
}
