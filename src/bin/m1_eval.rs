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
//! Counterfactual replay (`--override`) and log-driven input (`--log` CSV/`.ld`)
//! are Phase 3 and are not implemented here yet.

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
    if args.scenario.is_none() && !args.coverage {
        eprintln!("m1-eval: nothing to do (pass --scenario to run, or --coverage to report)");
        process::exit(2);
    }

    // Load the engine. A project/calibration that will not load is a fail-loud
    // run error (exit 1).
    let engine = match Engine::load(&project_path, args.config.as_deref()) {
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
