// SPDX-License-Identifier: GPL-3.0-or-later
//! The `m1-eval` command-line interface: a thin shell over the [`Engine`].
//!
//! Loads a project (and optional `.m1cfg` calibration), then either evaluates a
//! scenario into a [`Trace`] (written to `--out` as JSON or CSV, or to stdout) or
//! prints the static `--coverage` report. The heavy lifting lives in the library;
//! this binary only parses arguments, wires them to the engine, and maps results
//! onto the shared toolchain exit-code contract.
//!
//! ## Exit codes (shared toolchain contract, see `m1-tools/docs/cli.md`)
//!
//! - `0` — success: the run produced a trace, or the coverage report printed.
//! - `1` — the engine ran and has something to report: a project that would not
//!   load, a scenario that would not parse, or an evaluation error (a fail-loud
//!   `EvalError`).
//! - `2` — a usage error: an unrecognised flag, or arguments that do not name a
//!   runnable action (no resolvable project, or neither `--scenario` nor
//!   `--coverage`).
//!
//! Counterfactual replay is supported: `--log` attaches a recorded MoTeC log
//! (CSV, or `.ld` with `--features ld`) as ground truth, `--override CH=expr`
//! (repeatable) replaces channels, only their downstream cone recomputes, and
//! `--diff` writes the per-channel logged-vs-counterfactual delta.

use clap::{ArgGroup, Parser};
use m1_eval::{Engine, RunMode, Scenario};
use std::path::{Path, PathBuf};
use std::process;

/// Stepped, deterministic evaluator for MoTeC M1 scripts.
///
/// The three run-mode overrides (`--function`, `--target`, `--whole-project`)
/// are mutually exclusive — at most one selects how to drive the run. clap's
/// `ArgGroup` enforces this, exiting `2` (usage error) when more than one is
/// given, before the engine loads anything.
#[derive(Parser, Debug)]
#[command(
    name = "m1-eval",
    version,
    about = "Stepped, deterministic evaluator for MoTeC M1 scripts"
)]
#[command(group(
    ArgGroup::new("mode_override")
        .args(["function", "target", "whole_project"])
        .multiple(false)
))]
struct Args {
    /// Project.m1prj (defaults to the nearest one upward, or $M1_PROJECT).
    #[arg(long)]
    project: Option<PathBuf>,

    /// Calibration file (.m1cfg) supplying parameter values and table cells.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Scenario file (TOML or JSON) describing how to drive the run.
    #[arg(long)]
    scenario: Option<PathBuf>,

    /// Override the scenario's mode: run this single function each tick.
    /// Mutually exclusive with --target and --whole-project.
    #[arg(long)]
    function: Option<String>,

    /// Override the scenario's mode: run this target channel's dependency cone.
    /// Mutually exclusive with --function and --whole-project.
    #[arg(long)]
    target: Option<String>,

    /// Override the scenario's mode: run the whole-project multi-rate scheduler
    /// (every periodically-scheduled function at its own rate). Mutually
    /// exclusive with --function and --target.
    #[arg(long)]
    whole_project: bool,

    /// Where to write the trace. Format is inferred from the extension
    /// (.json or .csv); without --out the trace prints to stdout as JSON.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Print the coverage report (which constructs/builtins are supported,
    /// stubbed, or unsupported) instead of, or alongside, running.
    #[arg(long)]
    coverage: bool,

    /// Counterfactual replay: a recorded MoTeC log (CSV, or .ld with
    /// `--features ld`) held as ground truth. Triggers a counterfactual run.
    #[arg(long)]
    log: Option<PathBuf>,

    /// A counterfactual channel override `CHANNEL=value-or-expression` (repeatable).
    /// The channel is pinned to the constant or expression each tick; only its
    /// downstream cone recomputes. Requires --log.
    #[arg(long = "override", requires = "log")]
    overrides: Vec<String>,

    /// Where to write the counterfactual diff (per-channel logged-vs-counterfactual
    /// delta). Format inferred from the extension (.json or .csv). Requires --log.
    #[arg(long, requires = "log")]
    diff: Option<PathBuf>,
}

/// The output format for a written trace, inferred from the `--out` extension.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutFormat {
    Json,
    Csv,
}

/// Resolve the project path: explicit `--project`, then `$M1_PROJECT`, then the
/// nearest `Project.m1prj` upward from the cwd. Mirrors `m1-doc`.
fn resolve_project(arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return Some(p);
    }
    if let Ok(p) = std::env::var("M1_PROJECT") {
        return Some(PathBuf::from(p));
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("Project.m1prj");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Infer the trace format from an output path's extension. `.csv` → CSV; anything
/// else (including `.json` and no extension) → JSON.
fn format_for(out: &Path) -> OutFormat {
    match out.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("csv") => OutFormat::Csv,
        _ => OutFormat::Json,
    }
}

/// Read and parse a scenario file, picking the parser by extension (`.json` →
/// JSON, otherwise TOML). Returns a usage-style message string on read failure so
/// `main` can decide the exit code.
fn load_scenario(path: &Path) -> Result<Scenario, String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let is_json = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let parsed = if is_json {
        Scenario::from_json_str(&body)
    } else {
        Scenario::from_toml_str(&body)
    };
    parsed.map_err(|e| format!("{}: {e}", path.display()))
}

/// Apply a `--function`/`--target`/`--whole-project` mode override to a scenario,
/// if one was given. The CLI flag wins over the scenario file's `mode`/`target`.
/// At most one of the three can be set (enforced by the clap `ArgGroup`), so the
/// order of these branches is immaterial.
fn apply_mode_override(
    scenario: &mut Scenario,
    function: Option<String>,
    target: Option<String>,
    whole_project: bool,
) {
    if whole_project {
        scenario.mode = RunMode::WholeProject;
    } else if let Some(f) = function {
        scenario.mode = RunMode::Function(f);
    } else if let Some(t) = target {
        scenario.mode = RunMode::Cone(t);
    }
}

fn main() {
    let args = Args::parse();

    // Resolve the project; no project at all is a usage error (exit 2).
    let Some(project_path) = resolve_project(args.project) else {
        eprintln!("m1-eval: no Project.m1prj found (pass --project or set $M1_PROJECT)");
        process::exit(2);
    };

    // A run needs something to do: evaluate a scenario, or print coverage. With
    // neither, the invocation is incomplete — a usage error.
    if args.scenario.is_none() && !args.coverage && args.log.is_none() {
        eprintln!(
            "m1-eval: nothing to do (pass --scenario or --log to run, or --coverage to report)"
        );
        process::exit(2);
    }

    // Load the engine. A project/calibration that will not load is a fail-loud
    // run error (exit 1).
    let mut engine = match Engine::load(&project_path, args.config.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("m1-eval: {}: {e}", project_path.display());
            process::exit(1);
        }
    };

    // Coverage report (static analysis), if requested.
    if args.coverage {
        print!("{}", engine.coverage().render());
    }

    // Counterfactual replay (--log): hold the log as ground truth, apply the
    // overrides, recompute only the downstream cone, and emit the trace (+ diff).
    if let Some(log_path) = &args.log {
        if let Err(e) = engine.load_log(log_path) {
            eprintln!("m1-eval: {}: {e}", log_path.display());
            process::exit(1);
        }
        for spec in &args.overrides {
            if let Err(e) = engine.override_channel(spec) {
                eprintln!("m1-eval: --override {spec:?}: {e}");
                process::exit(1);
            }
        }
        let cf = match engine.run_counterfactual_diff() {
            Ok(cf) => cf,
            Err(e) => {
                eprintln!("m1-eval: {e}");
                process::exit(1);
            }
        };
        // The recomputed trace -> --out (or stdout as JSON).
        match &args.out {
            Some(out) => {
                let body = match format_for(out) {
                    OutFormat::Json => cf.trace.to_json(),
                    OutFormat::Csv => cf.trace.to_csv(),
                };
                if let Err(e) = std::fs::write(out, body) {
                    eprintln!("m1-eval: {}: {e}", out.display());
                    process::exit(1);
                }
            }
            None => println!("{}", cf.trace.to_json()),
        }
        // The per-channel diff -> --diff (format by extension), if requested.
        if let Some(diff_path) = &args.diff {
            let body = match format_for(diff_path) {
                OutFormat::Json => cf.diff.to_json(),
                OutFormat::Csv => cf.diff.to_csv(),
            };
            if let Err(e) = std::fs::write(diff_path, body) {
                eprintln!("m1-eval: {}: {e}", diff_path.display());
                process::exit(1);
            }
        }
        return;
    }

    // Evaluate the scenario, if given.
    if let Some(scenario_path) = args.scenario {
        let mut scenario = match load_scenario(&scenario_path) {
            Ok(s) => s,
            Err(msg) => {
                eprintln!("m1-eval: {msg}");
                process::exit(1);
            }
        };
        apply_mode_override(
            &mut scenario,
            args.function,
            args.target,
            args.whole_project,
        );

        let trace = match engine.run(&scenario) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("m1-eval: {e}");
                process::exit(1);
            }
        };

        match args.out {
            Some(out) => {
                let body = match format_for(&out) {
                    OutFormat::Json => trace.to_json(),
                    OutFormat::Csv => trace.to_csv(),
                };
                if let Err(e) = std::fs::write(&out, body) {
                    eprintln!("m1-eval: {}: {e}", out.display());
                    process::exit(1);
                }
            }
            // No --out: print JSON to stdout.
            None => println!("{}", trace.to_json()),
        }
    }
}
