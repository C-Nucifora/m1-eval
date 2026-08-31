// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`Engine`]: the public library facade over the loader, runners, and
//! coverage analysis.
//!
//! `Engine` is the one entry point a consumer (the visualiser, the CLI, a later
//! LSP) uses. It owns the loaded project internally and exposes only `m1-eval`'s
//! own types: [`Scenario`], [`Trace`], [`CoverageReport`], [`EvalError`], and
//! [`HardwareAdapter`]. No
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
use crate::hardware::HardwareAdapter;
use crate::loader::{Loaded, load};
use crate::log::Log;
use crate::runner::{
    CounterfactualCfg, run as run_scenario, run_counterfactual,
    run_counterfactual_with_adapter as replay_with_adapter,
    run_with_adapter as run_scenario_with_adapter,
};
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
    /// Reads the file as raw bytes and hands them to the clean-room binary reader
    /// ([`crate::log::ld::from_ld`], built on the MIT `motec-i2` crate), which
    /// applies the engineering-unit scaling and derives each sample's time from the
    /// channel sample rate. All `motec-i2` types stay inside that module; only the
    /// `m1-eval` [`Log`] crosses back here.
    #[cfg(feature = "ld")]
    fn load_ld(path: &Path, source: String) -> Result<Log, EvalError> {
        let bytes = std::fs::read(path).map_err(|e| EvalError::MissingInput {
            channel: format!("{}: {e}", path.display()),
        })?;
        crate::log::ld::from_ld(&bytes, source)
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
    /// truth to replay, which fails loud. The base tick rate defaults to the least
    /// common multiple of the project's scheduled call rates — the smallest grid
    /// every cone function's declared rate divides exactly (100 Hz when the project
    /// schedules nothing periodically); the duration defaults to the log's own
    /// duration. Each cone function keeps its declared rate on that grid.
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

    /// Run the attached counterfactual log through an external hardware adapter.
    /// Hardware calls in override expressions and recomputed scripts share the
    /// same adapter and deterministic replay timeline.
    pub fn run_counterfactual_with_adapter(
        &self,
        hardware: &mut dyn HardwareAdapter,
    ) -> Result<Trace, EvalError> {
        let log = self.log.as_ref().ok_or_else(|| EvalError::MissingInput {
            channel: "counterfactual run needs a log: call load_log first".to_string(),
        })?;
        let cfg = CounterfactualCfg {
            base_rate_hz: self.default_counterfactual_rate(),
            duration_s: 0.0,
        };
        replay_with_adapter(&self.loaded, log, &self.overrides, &cfg, hardware)
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

    /// The default base tick rate for a counterfactual run: the least common multiple
    /// of the project's periodic call rates, or 100 Hz when no exact common base
    /// exists or no function schedules periodically. A counterfactual recomputes only
    /// the override cone, but the grid rate governs stateful-operator `dt`, so the
    /// project-derived rate is the documented model.
    fn default_counterfactual_rate(&self) -> f64 {
        // The lcm of the declared rates — the smallest grid every cone function
        // divides exactly, mirroring the whole-project auto base. The fastest
        // rate is NOT sufficient: rates {10, 4} need a 20 Hz grid, and the
        // replay preserves each cone function's declared rate, so the base must
        // represent them all. 100 Hz when the project schedules nothing
        // periodically (or no exact common base exists under the 1 MHz cap).
        crate::runner::lcm_rate_hz(self.loaded.scripts.iter().filter_map(|script| {
            let fn_symbol = self
                .loaded
                .project
                .function_symbol_for_script(&script.name)?;
            self.loaded.triggers.periodic_rate(&fn_symbol)
        }))
        .unwrap_or(100.0)
    }

    /// Evaluate a scenario, producing a [`Trace`] of channel/expression values
    /// over the scenario's tick grid. Dispatches single-function, dependency-cone,
    /// or the whole-project multi-rate scheduler per the scenario's mode.
    /// Deterministic.
    pub fn run(&self, scenario: &Scenario) -> Result<Trace, EvalError> {
        run_scenario(&self.loaded, scenario)
    }

    /// Evaluate a scenario with an external typed hardware adapter.
    ///
    /// The adapter is borrowed only for this run and may keep mutable state.
    /// Exact-site and wildcard scenario IO values take precedence over it.
    pub fn run_with_adapter(
        &self,
        scenario: &Scenario,
        hardware: &mut dyn HardwareAdapter,
    ) -> Result<Trace, EvalError> {
        run_scenario_with_adapter(&self.loaded, scenario, hardware)
    }

    /// Report which builtins/constructs every loaded script uses and whether the
    /// engine supports, assumes, routes through a typed adapter, stubs, or cannot
    /// handle each, along with the whole-project execution schedule. Adapter
    /// routes may be user-supplied or evaluator-owned. Pure static analysis; no
    /// scenario is needed, so this is safe before [`Engine::run`].
    pub fn coverage(&self) -> CoverageReport {
        CoverageReport::analyse_loaded(&self.loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{
        AdapterReply, EvalPhase, HardwareAdapter, HardwareCall, HardwareValueSource,
        ResolvedReceiver,
    };
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

    fn hardware_engine() -> Engine {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hardware");
        Engine::load(&dir.join("Project.m1prj"), None).expect("hardware fixture loads")
    }

    fn hardware_scenario(extra: &str) -> Scenario {
        Scenario::from_toml_str(&format!(
            "mode = \"function\"\ntarget = \"Hardware.Update\"\nduration_s = 0.03\nbase_rate_hz = 100.0\n{extra}"
        ))
        .expect("hardware scenario parses")
    }

    #[derive(Default)]
    struct MetadataAdapter {
        calls: Vec<HardwareCall>,
    }

    impl HardwareAdapter for MetadataAdapter {
        fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError> {
            self.calls.push(call.clone());
            Ok(match call.method.as_str() {
                "FlashSize" => AdapterReply::Value(Value::m1_integer(8_388_608)),
                "FlashFree" => AdapterReply::Value(Value::m1_integer(2_097_152)),
                _ => AdapterReply::Unhandled,
            })
        }
    }

    #[test]
    fn engine_adapter_drives_required_metadata_and_system_uses_the_tick_grid() {
        let engine = hardware_engine();
        let scenario = hardware_scenario("");
        let mut adapter = MetadataAdapter::default();
        let trace = engine
            .run_with_adapter(&scenario, &mut adapter)
            .expect("adapter-backed hardware run succeeds");

        assert_eq!(
            trace.channels["Root.Hardware.Elapsed"],
            vec![
                Value::m1_float(0.0),
                Value::m1_float(0.01),
                Value::m1_float(0.01),
            ]
        );
        assert_eq!(
            trace.channels["Root.Hardware.Period"],
            vec![Value::m1_float(0.01); 3]
        );
        assert_eq!(
            trace.channels["Root.Hardware.Tick"],
            vec![
                Value::m1_unsigned(0),
                Value::m1_unsigned(1),
                Value::m1_unsigned(2),
            ]
        );
        for channel in ["FlashSizeA", "FlashSizeB"] {
            assert_eq!(
                trace.channels[&format!("Root.Hardware.{channel}")],
                vec![Value::m1_unsigned(8_388_608); 3]
            );
        }
        assert_eq!(
            trace.channels["Root.Hardware.FlashFreeValue"],
            vec![Value::m1_unsigned(2_097_152); 3]
        );

        let metadata: Vec<&HardwareCall> = adapter
            .calls
            .iter()
            .filter(|call| matches!(call.method.as_str(), "FlashSize" | "FlashFree"))
            .collect();
        assert_eq!(
            metadata.len(),
            9,
            "three metadata calls on each of three ticks"
        );
        assert!(metadata.iter().all(|call| {
            call.receiver
                == ResolvedReceiver::Library {
                    object: "System".to_string(),
                }
                && call.source_receiver == "System"
        }));
        assert_eq!(
            metadata
                .iter()
                .map(|call| call.time.base_tick)
                .collect::<Vec<_>>(),
            vec![0, 0, 0, 1, 1, 1, 2, 2, 2]
        );
        assert!(trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::Adapter
                && record.canonical_call() == "System.FlashSize"
        }));
        assert!(trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::SystemModel
                && record.canonical_call() == "System.ElapsedTime"
        }));

        let json: serde_json::Value =
            serde_json::from_str(&trace.to_json()).expect("hardware trace is valid JSON");
        let hardware = json["hardware"]
            .as_array()
            .expect("hardware provenance array");
        assert!(hardware.iter().any(|record| {
            record["receiver"]["kind"] == "library"
                && record["receiver"]["name"] == "System"
                && record["source_call"] == "System.FlashSize"
                && record["method"] == "FlashSize"
                && record["script"] == "Hardware.Update.m1scr"
                && record["source"] == "adapter"
                && record["offset"].is_u64()
        }));
        assert!(hardware.iter().any(|record| {
            record["source_call"] == "System.ElapsedTime" && record["source"] == "system-model"
        }));
    }

    #[test]
    fn startup_hardware_calls_receive_startup_time_and_keep_provenance() {
        let engine = hardware_engine();
        let scenario = Scenario::from_toml_str(
            "mode = \"whole-project\"\nduration_s = 0.03\nbase_rate_hz = 100.0\n",
        )
        .expect("whole-project hardware scenario parses");
        let mut adapter = MetadataAdapter::default();
        let trace = engine
            .run_with_adapter(&scenario, &mut adapter)
            .expect("whole-project hardware run succeeds");

        assert_eq!(
            trace.channels["Root.Hardware.StartupTick"],
            vec![Value::m1_unsigned(0); 3]
        );
        assert_eq!(
            trace.channels["Root.Hardware.StartupElapsed"],
            vec![Value::m1_float(0.0); 3]
        );
        let startup = adapter
            .calls
            .iter()
            .find(|call| call.site.script() == "Hardware.Init.m1scr")
            .expect("startup call reaches the adapter before the System model");
        assert_eq!(startup.method, "Ticks");
        assert_eq!(startup.time.phase, EvalPhase::Startup);
        assert_eq!(startup.time.base_tick, 0);
        assert_eq!(startup.time.elapsed_s, 0.0);
        assert_eq!(startup.time.base_period_s, 0.01);
        assert_eq!(startup.time.step_s, 0.01);
        assert!(trace.hardware.iter().any(|record| {
            record.site.script() == "Hardware.Init.m1scr"
                && record.source == HardwareValueSource::SystemModel
                && record.canonical_call() == "System.Ticks"
        }));
    }

    #[test]
    fn rate_gated_elapsed_time_uses_actual_execution_instants_and_resets_per_run() {
        let engine = hardware_engine();
        let scenario = Scenario::from_toml_str(
            "mode = \"whole-project\"\nduration_s = 0.05\nbase_rate_hz = 100.0\n",
        )
        .expect("whole-project hardware scenario parses");
        let expected = vec![
            Value::m1_float(0.0),
            Value::m1_float(0.0),
            Value::m1_float(0.02),
            Value::m1_float(0.02),
            Value::m1_float(0.02),
        ];

        let mut first_adapter = MetadataAdapter::default();
        let first = engine
            .run_with_adapter(&scenario, &mut first_adapter)
            .expect("first rate-gated run succeeds");
        assert_eq!(first.channels["Root.Hardware.SlowElapsed"], expected);
        let slow_calls: Vec<&HardwareCall> = first_adapter
            .calls
            .iter()
            .filter(|call| {
                call.method == "ElapsedTime" && call.site.script() == "Hardware.Slow.m1scr"
            })
            .collect();
        assert_eq!(
            slow_calls
                .iter()
                .map(|call| call.time.base_tick)
                .collect::<Vec<_>>(),
            vec![0, 2, 4]
        );
        assert!(slow_calls.iter().all(|call| call.time.step_s == 0.02));

        let mut second_adapter = MetadataAdapter::default();
        let second = engine
            .run_with_adapter(&scenario, &mut second_adapter)
            .expect("fresh run succeeds");
        assert_eq!(
            second.channels["Root.Hardware.SlowElapsed"], expected,
            "a new run must not retain the first run's call-site epoch"
        );
    }

    #[test]
    fn missing_flash_metadata_aborts_with_script_and_tick_context() {
        let engine = hardware_engine();
        let error = engine.run(&hardware_scenario("")).unwrap_err();
        assert_eq!(
            error.root_cause(),
            &EvalError::MissingHardwareMetadata {
                call: "System.FlashSize".to_string()
            }
        );
        let message = error.to_string();
        assert!(message.contains("Hardware.Update.m1scr"), "{message}");
        assert!(message.contains("t = 0.000 s"), "{message}");
        assert!(message.contains("[[io]]"), "{message}");
    }

    #[test]
    fn scenario_exact_site_override_does_not_collide_with_the_other_call() {
        let engine = hardware_engine();
        let source = include_str!("../tests/fixtures/hardware/Scripts/Hardware.Update.m1scr");
        let first_offset = source
            .match_indices("System.FlashSize")
            .next()
            .expect("first FlashSize call")
            .0;
        let extra = format!(
            r#"

[[io]]
call = "System.FlashSize"
const = 4194304

[[io]]
call = "System.FlashSize"
script = "Hardware.Update.m1scr"
offset = {first_offset}
const = 8388608

[[io]]
call = "System.FlashFree"
const = 2097152
"#
        );
        let trace = engine
            .run(&hardware_scenario(&extra))
            .expect("site-specific scenario run succeeds");

        assert_eq!(
            trace.channels["Root.Hardware.FlashSizeA"],
            vec![Value::m1_unsigned(8_388_608); 3]
        );
        assert_eq!(
            trace.channels["Root.Hardware.FlashSizeB"],
            vec![Value::m1_unsigned(4_194_304); 3]
        );
        assert!(trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::ScenarioExact
                && record.site.offset() == first_offset
        }));
        assert!(trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::ScenarioWildcard
                && record.canonical_call() == "System.FlashSize"
        }));
    }

    #[test]
    fn default_counterfactual_base_covers_non_dividing_rates() {
        // The cfrate fixture schedules 10 Hz and 4 Hz functions, both downstream
        // of Sensor. The default counterfactual base must be a rate BOTH divide
        // exactly — their lcm, 20 Hz. The old "fastest declared rate" default
        // (10 Hz) cannot represent the 4 Hz cone function (10/4 = 2.5) and the
        // replay would fail loud.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cfrate");
        let mut engine = Engine::load(&dir.join("Project.m1prj"), None).expect("cfrate loads");
        let csv = "time,Root.CR.Sensor\n0.0,5.0\n0.5,5.0\n1.0,5.0\n";
        let (_dir, path) = temp_log("csv", csv);
        engine.load_log(&path).expect("log loads");
        engine
            .override_channel("Root.CR.Sensor=7.0")
            .expect("override parses");
        let trace = engine
            .run_counterfactual()
            .expect("default base must schedule both 10 and 4 Hz exactly");
        assert!(!trace.time.is_empty());
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
        assert_eq!(out, &vec![Value::m1_float(50.0); 3]);
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
        assert_eq!(echo, &vec![Value::m1_float(6.0); 4]);
        // The On-Startup function ran exactly once before the periodic loop;
        // its marker holds across every tick.
        let started = trace.channels.get("Root.MR.Started").expect("Started");
        assert_eq!(started, &vec![Value::m1_float(1.0); 4]);
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

    // Built WITH the `ld` feature, an `.ld` path routes through the clean-room
    // reader (`Log::from_ld`) and attaches scaled channels as ground truth. The
    // synthetic `.ld` is written in-memory via the `motec-i2` writer (no
    // proprietary bytes) to a temp file, then loaded back through `load_log`.
    #[cfg(feature = "ld")]
    #[test]
    fn load_log_ld_with_feature_attaches_scaled_channels() {
        use motec_i2::{ChannelMetadata, Datatype, Header, LDWriter, Sample};
        use std::io::Cursor;

        // The writer places the first channel block at this fixed offset; the
        // header must advertise it so the reader's linked-list walk finds it.
        let header = Header {
            channel_meta_ptr: 0x3448,
            channel_data_ptr: 0,
            event_ptr: 0,
            device_serial: 1,
            device_type: "M1".to_string(),
            device_version: 1,
            num_channels: 1,
            date_string: "23/06/2026".to_string(),
            time_string: "00:00:00".to_string(),
            driver: "synthetic".to_string(),
            vehicleid: "EV25".to_string(),
            venue: "synthetic".to_string(),
            session: "synthetic".to_string(),
            short_comment: "m1-eval synthetic fixture".to_string(),
        };
        // I16 @ 10 Hz, scale=1/dec_places=1/mul=1 -> raw * 0.1.
        let meta = ChannelMetadata {
            prev_addr: 0,
            next_addr: 0,
            data_addr: 0,
            data_count: 0,
            datatype: Datatype::I16,
            sample_rate: 10,
            offset: 0,
            mul: 1,
            scale: 1,
            dec_places: 1,
            name: "Sensor".to_string(),
            short_name: "Sensor".to_string(),
            unit: "V".to_string(),
        };
        let mut cursor = Cursor::new(Vec::new());
        LDWriter::new(&mut cursor, header)
            .with_channel(meta, vec![Sample::I16(100), Sample::I16(200)])
            .write()
            .expect("synthetic .ld writes");
        let bytes = cursor.into_inner();

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("run.ld");
        std::fs::write(&path, &bytes).expect("write synthetic .ld");

        let mut engine = mini_engine();
        engine
            .load_log(&path)
            .expect(".ld log attaches via from_ld");

        let log = engine.log().expect("log attached after load_log");
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Sensor"]);
        let sensor = log.series_for("Sensor").expect("Sensor present");
        // Engineering-unit scaling applied: raw 100 -> 10.0 at t=0.0.
        assert_eq!(sensor.sample(0.0), Value::m1_float(10.0));
        // Time grid derived from the 10 Hz rate: second sample at t=0.1 -> 20.0.
        assert_eq!(sensor.sample(0.1), Value::m1_float(20.0));
        // Units rode along into provenance metadata.
        assert_eq!(log.meta.units.get("Sensor").map(String::as_str), Some("V"));
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
        assert!(mid.iter().all(|v| *v == Value::m1_float(200.0)), "{mid:?}");
        assert!(
            result.iter().all(|v| *v == Value::m1_float(201.0)),
            "{result:?}"
        );
        // Other is unrelated to the override: it passes through at its logged value.
        assert!(
            other.iter().all(|v| *v == Value::m1_float(42.0)),
            "{other:?}"
        );
    }

    #[test]
    fn counterfactual_override_expression_uses_the_hardware_adapter_timeline() {
        let mut engine = cf_engine();
        let (_dir, path) = temp_log("csv", CF_LOG_CSV);
        engine.load_log(&path).expect("log attaches");
        engine
            .override_channel("Root.CF.Sensor=System.FlashSize()")
            .expect("hardware expression override parses");
        let mut adapter = MetadataAdapter::default();

        let trace = engine
            .run_counterfactual_with_adapter(&mut adapter)
            .expect("counterfactual hardware expression runs");
        assert_eq!(trace.time, vec![0.0, 0.01]);
        let flash_calls: Vec<&HardwareCall> = adapter
            .calls
            .iter()
            .filter(|call| call.method == "FlashSize")
            .collect();
        assert_eq!(flash_calls.len(), 2);
        assert_eq!(
            flash_calls[0].site.script(),
            "<counterfactual-override:0:Root.CF.Sensor>"
        );
        assert_eq!(flash_calls[0].time.base_tick, 0);
        assert_eq!(flash_calls[1].time.base_tick, 1);
        assert!(
            flash_calls
                .iter()
                .all(|call| call.time.phase == EvalPhase::Periodic)
        );
        assert!(trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::Adapter
                && record.site.script() == "<counterfactual-override:0:Root.CF.Sensor>"
                && record.canonical_call() == "System.FlashSize"
        }));
    }

    #[test]
    fn identical_counterfactual_hardware_expressions_keep_distinct_call_sites() {
        let mut engine = cf_engine();
        let (_dir, path) = temp_log("csv", CF_LOG_CSV);
        engine.load_log(&path).expect("log attaches");
        engine
            .override_channel("Root.CF.Sensor=System.ElapsedTime()")
            .expect("first hardware expression override parses");
        engine
            .override_channel("Root.CF.Mid=System.ElapsedTime()")
            .expect("second hardware expression override parses");
        let mut adapter = MetadataAdapter::default();

        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.03,
        };
        let trace = crate::runner::run_counterfactual_with_adapter(
            &engine.loaded,
            engine.log().expect("log remains attached"),
            &engine.overrides,
            &cfg,
            &mut adapter,
        )
        .expect("counterfactual hardware expressions run");
        let elapsed_calls: Vec<&HardwareCall> = adapter
            .calls
            .iter()
            .filter(|call| call.method == "ElapsedTime")
            .collect();
        assert_eq!(elapsed_calls.len(), 6, "two overrides on three ticks");
        let sites: std::collections::BTreeSet<_> =
            elapsed_calls.iter().map(|call| call.site.clone()).collect();
        assert_eq!(sites.len(), 2);
        assert_eq!(
            sites.iter().map(|site| site.script()).collect::<Vec<_>>(),
            vec![
                "<counterfactual-override:0:Root.CF.Sensor>",
                "<counterfactual-override:1:Root.CF.Mid>",
            ]
        );
        assert_eq!(
            sites
                .iter()
                .map(|site| site.offset())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            1,
            "the identical expression layout differs only by override identity"
        );
        assert_eq!(
            trace.channels["Root.CF.Sensor"],
            vec![
                Value::m1_float(0.0),
                Value::m1_float(0.01),
                Value::m1_float(0.01),
            ],
            "counterfactual calls keep previous-execution interval state"
        );

        let provenance: Vec<_> = trace
            .hardware
            .iter()
            .filter(|record| record.canonical_call() == "System.ElapsedTime")
            .collect();
        assert_eq!(provenance.len(), 2);
        assert!(
            provenance
                .iter()
                .all(|record| record.source == HardwareValueSource::SystemModel)
        );
        assert_eq!(
            provenance
                .iter()
                .map(|record| record.site.clone())
                .collect::<std::collections::BTreeSet<_>>(),
            sites
        );
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
