// SPDX-License-Identifier: GPL-3.0-or-later
//! Environment-gated EV and AV acceptance smoke tests.
//!
//! These load real proprietary projects from local paths given by
//! `M1_EVAL_EVM1_DIR` and `M1_EVAL_AVM1_DIR`. A normal test run executes the
//! gates, but each gate skips with a message when its corpus variable is absent.
//! Point either variable at the repository root, version directory, or exact
//! `Project.m1prj`:
//!
//! ```text
//! M1_EVAL_EVM1_DIR=/path/to/EV-M1 \
//! M1_EVAL_AVM1_DIR=/path/to/av-firmware \
//!   cargo test --test evm1_smoke -- --nocapture
//! ```
//!
//! The tests discover the versioned project below that path and the matching
//! repository-level `parameters.m1cfg`. No corpus files are modified or copied.
//!
//! ## Phase-1.5 acceptance gate ([`evm1_phase15_categories_are_closed`])
//!
//! After P15-A…D, the `--coverage` Unsupported list must no longer contain any of
//! the categories Phase 1.5 closed: pure `Calculate.*` overloads, enum
//! `.AsInteger`, project-object `.Set`/`.Update` methods, or inline user-function
//! calls. This test asserts exactly that against the real project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use m1_can::{CanDirection, CanFrameFormat};
use m1_core::{Field, Kind, Node};
use m1_eval::{
    CanRx, Engine, Env, EvalCtx, FixedPoint7dps, HardwareValueSource, InputKind, InputSeries,
    Loaded, M1Scalar, Scenario, StateStore, Trace, Value, eval, io_sets, load,
};
use m1_typecheck::parsed::ParsedScript;
use m1_typecheck::symbols::{Symbol, SymbolKind};
use m1_typecheck::types::ValueType;

const EV_ENV: &str = "M1_EVAL_EVM1_DIR";
const AV_ENV: &str = "M1_EVAL_AVM1_DIR";

#[derive(Debug)]
struct CorpusLayout {
    project: PathBuf,
    config: PathBuf,
}

/// Resolve the EV-M1 corpus hint. `None` makes the test print its explicit
/// no-corpus skip reason before returning.
fn evm1_dir() -> Option<PathBuf> {
    std::env::var_os(EV_ENV).map(PathBuf::from)
}

fn corpus_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from)
}

/// Resolve the layouts used by the real repositories. The project sits below a
/// version directory while `parameters.m1cfg` sits at the repository root.
/// Callers may point the environment variable at either level.
fn discover_corpus(path: &Path) -> Result<CorpusLayout, String> {
    let project = if path.is_file() {
        if path.file_name().and_then(|name| name.to_str()) != Some("Project.m1prj") {
            return Err(format!("{} is not a Project.m1prj", path.display()));
        }
        path.to_path_buf()
    } else {
        let direct = path.join("Project.m1prj");
        if direct.is_file() {
            direct
        } else {
            let mut projects = Vec::new();
            collect_named(path, "Project.m1prj", &mut projects)?;
            projects.sort();
            match projects.as_slice() {
                [project] => project.clone(),
                [] => return Err(format!("{} contains no Project.m1prj", path.display())),
                _ => {
                    return Err(format!(
                        "{} contains {} Project.m1prj files; point the variable at one version",
                        path.display(),
                        projects.len()
                    ));
                }
            }
        }
    };

    // The checked corpora keep their calibration two levels above the project.
    // Walk a few ancestors first, then fall back to a unique recursive `.m1cfg`.
    let mut config = project.parent().and_then(|parent| {
        parent
            .ancestors()
            .take(4)
            .map(|ancestor| ancestor.join("parameters.m1cfg"))
            .find(|candidate| candidate.is_file())
    });
    if config.is_none() {
        let search_root = if path.is_file() {
            path.parent().unwrap_or_else(|| Path::new("."))
        } else {
            path
        };
        let mut configs = Vec::new();
        collect_extension(search_root, "m1cfg", &mut configs)?;
        configs.sort();
        if let [candidate] = configs.as_slice() {
            config = Some(candidate.clone());
        }
    }
    let config =
        config.ok_or_else(|| format!("no unambiguous .m1cfg found for {}", project.display()))?;

    Ok(CorpusLayout { project, config })
}

fn collect_named(dir: &Path, name: &str, out: &mut Vec<PathBuf>) -> Result<(), String> {
    collect_files(dir, out, &|path| {
        path.file_name().and_then(|file| file.to_str()) == Some(name)
    })
}

fn collect_extension(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) -> Result<(), String> {
    collect_files(dir, out, &|path| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
    })
}

fn collect_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    matches: &dyn Fn(&Path) -> bool,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            if !matches!(entry.file_name().to_str(), Some(".git" | "target")) {
                collect_files(&path, out, matches)?;
            }
        } else if file_type.is_file() && matches(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// Load one corpus with the configuration discovered from its real layout.
fn load_corpus(dir: &Path, label: &str) -> Engine {
    let layout = discover_corpus(dir)
        .unwrap_or_else(|error| panic!("cannot discover {label} corpus: {error}"));
    eprintln!(
        "{label}: project={}, config={}",
        layout.project.display(),
        layout.config.display()
    );
    Engine::load(&layout.project, Some(&layout.config))
        .unwrap_or_else(|error| panic!("{label} project and configuration load: {error}"))
}

fn load_evm1(dir: &Path) -> Engine {
    load_corpus(dir, "EV")
}

#[test]
fn evm1_phase15_categories_are_closed() {
    let Some(dir) = evm1_dir() else {
        eprintln!("M1_EVAL_EVM1_DIR unset; skipping EV-M1 Phase-1.5 coverage gate");
        return;
    };
    let engine = load_evm1(&dir);
    let report = engine.coverage();

    // Every Phase-1.5 category must be absent from the Unsupported list. We check
    // by the item *name* spelling so a regression in any one category is pinpointed.
    let unsupported: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();

    // 1. Pure Calculate.* overloads (P15-A).
    let calc: Vec<&&str> = unsupported
        .iter()
        .filter(|n| n.starts_with("Calculate."))
        .collect();
    assert!(
        calc.is_empty(),
        "Calculate.* overloads still unsupported: {calc:?}"
    );

    // 2. Enum .AsInteger (P15-B).
    let as_int: Vec<&&str> = unsupported
        .iter()
        .filter(|n| n.ends_with(".AsInteger"))
        .collect();
    assert!(
        as_int.is_empty(),
        ".AsInteger conversions still unsupported: {as_int:?}"
    );

    // 3. Project-object setters / IO writers (P15-C): `<obj>.Set` and `<obj>.Update`.
    let set_update: Vec<&&str> = unsupported
        .iter()
        .filter(|n| n.ends_with(".Set") || n.ends_with(".Update"))
        .collect();
    assert!(
        set_update.is_empty(),
        "project-object .Set/.Update still unsupported: {set_update:?}"
    );

    // 4. Inline user-function calls (P15-D): the two EV-M1 control helpers must be
    //    Supported, never Unsupported (they classify as user functions now).
    for user_fn in ["Slip Control.Update", "Torque Vectoring.Update"] {
        assert!(
            !unsupported.contains(&user_fn),
            "user function {user_fn:?} still unsupported; unsupported={unsupported:?}"
        );
    }
}

/// Issue #41 corpus gate: the four suspension update functions inherit their
/// trigger from each sensor's `Absolute Travel.Calculation` component. All four
/// must resolve to the effective 200 Hz rate instead of appearing unresolved or
/// unscheduled.
#[test]
fn evm1_parameterized_suspension_triggers_resolve_to_200_hz() {
    let Some(dir) = evm1_dir() else {
        eprintln!("M1_EVAL_EVM1_DIR unset; skipping EV-M1 trigger-resolution gate");
        return;
    };
    let report = load_evm1(&dir).coverage();
    let by_function: std::collections::HashMap<&str, Option<f64>> = report
        .schedule
        .iter()
        .map(|(function, rate)| (function.as_str(), *rate))
        .collect();
    for function in [
        "Root.Vehicle.Suspension.Front.Left.Linpot.Update",
        "Root.Vehicle.Suspension.Front.Right.Linpot.Update",
        "Root.Vehicle.Suspension.Rear.Left.Linpot.Update",
        "Root.Vehicle.Suspension.Rear.Right.Linpot.Update",
    ] {
        assert_eq!(
            by_function.get(function),
            Some(&Some(200.0)),
            "{function} must inherit its 200 Hz Calculation trigger"
        );
        assert!(
            report
                .unresolved
                .iter()
                .all(|entry| entry.function != function),
            "{function} must not remain unresolved"
        );
    }
}

/// Phase-2 acceptance gate ([`evm1_whole_project_runs_end_to_end`]).
///
/// Loads the real EV-M1 project and runs the **whole-project multi-rate
/// scheduler** for a short fixed duration. This is the strongest end-to-end check
/// that Phase 1.5 + Phase 2 together make the real corpus runnable: every
/// periodically-scheduled function executes at its own rate, the inline
/// user-function calls evaluate, the enum `.AsInteger` conversions resolve, and
/// the externally-driven CAN/sensor IO falls back to its documented stubs — all
/// without a single fail-loud `EvalError`.
///
/// External values use typed IO defaults plus zero-filled scenario frames
/// derived from the loaded DBC snapshot. This exercises the real virtual-CAN
/// lifecycle instead of intercepting `.Receive()` before the model can update
/// its current message buffers. We assert the trace is non-empty and disclose
/// every ordinary, calibration, and receive substitution.
#[test]
fn evm1_whole_project_runs_end_to_end() {
    let Some(dir) = evm1_dir() else {
        eprintln!("M1_EVAL_EVM1_DIR unset; skipping EV-M1 whole-project smoke");
        return;
    };
    run_whole_project_smoke(&dir, "EV", 0.02);
}

#[test]
fn avm1_whole_project_runs_end_to_end() {
    let Some(dir) = corpus_dir(AV_ENV) else {
        eprintln!("M1_EVAL_AVM1_DIR unset; skipping AV-M1 whole-project smoke");
        return;
    };
    run_whole_project_smoke(&dir, "AV", 0.02);
}

/// Focused real-corpus CAN gate. This proves only that one real project
/// function routes its twelve DBC module initializers through VirtualCan. It is
/// not an RX/TX codec or whole-project conformance claim.
#[test]
fn avm1_can_init_function_routes_every_dbc_module_through_virtual_can() {
    let Some(dir) = corpus_dir(AV_ENV) else {
        eprintln!("M1_EVAL_AVM1_DIR unset; skipping AV-M1 focused CAN init smoke");
        return;
    };
    let layout = discover_corpus(&dir).expect("discover AV corpus for focused CAN smoke");
    let engine = Engine::load(&layout.project, Some(&layout.config))
        .expect("load AV corpus for focused CAN smoke");
    let scenario = Scenario::from_toml_str(
        r#"
mode = "function"
target = "Root.CAN.CAN Init"
duration_s = 0.001
base_rate_hz = 1000.0

[[inputs]]
channel = "Root.CAN.Active Bus"
const = { integer = 0 }

[[inputs]]
channel = "Root.CAN.Datalogger Bus"
const = { integer = 2 }
"#,
    )
    .expect("focused CAN init scenario parses");
    let trace = engine
        .run(&scenario)
        .expect("real CAN init function runs through VirtualCan");
    let initializers = trace
        .hardware
        .iter()
        .filter(|item| item.method == "Init" && item.source_call.starts_with("DBC."))
        .collect::<Vec<_>>();
    assert_eq!(
        initializers.len(),
        12,
        "the real CAN init function contains twelve DBC module calls"
    );
    assert!(
        initializers
            .iter()
            .all(|item| item.source == HardwareValueSource::VirtualCan),
        "every DBC module Init must use the run-owned CAN model: {initializers:#?}"
    );
    assert!(trace.hardware.iter().all(|item| {
        !(item.method == "Init"
            && item.source_call.starts_with("DBC.")
            && item.source == HardwareValueSource::GenericStub)
    }));
}

fn run_whole_project_smoke(dir: &Path, label: &str, duration_s: f64) -> Trace {
    let layout = discover_corpus(dir)
        .unwrap_or_else(|error| panic!("cannot discover {label} corpus: {error}"));
    let loaded = load(&layout.project, Some(&layout.config))
        .unwrap_or_else(|error| panic!("{label} project and configuration load: {error}"));
    let calibration_inputs = neutralize_zero_calibration_denominators(&loaded);
    let engine = load_corpus(dir, label);
    let scheduled_rates = engine
        .coverage()
        .schedule
        .into_iter()
        .filter_map(|(_, rate)| rate)
        .collect::<Vec<_>>();
    assert!(!scheduled_rates.is_empty(), "{label}: no periodic schedule");

    // Leave base_rate_hz absent. The runner derives the exact LCM, including the
    // 200 Hz jobs which a fixed 500 Hz grid cannot represent.
    let mut scenario = Scenario::from_toml_str(&format!(
        r#"
mode = "whole-project"
duration_s = {duration_s}
allow_default_inputs = true

[[io]]
call = "System.FlashSize"
const = {{ unsigned = 8388608 }}

[[io]]
call = "System.FlashFree"
const = {{ unsigned = 2097152 }}
"#
    ))
    .expect("whole-project smoke scenario parses");
    scenario.inputs.extend(calibration_inputs);
    scenario.can.rx.extend(zero_can_frames(&loaded));
    eprintln!(
        "{label}: {} neutralized zero-denominator calibration input(s)",
        scenario.inputs.len()
    );
    for input in &scenario.inputs {
        eprintln!("  {} = {:?}", input.channel, input.kind);
    }
    assert_eq!(
        scenario.base_rate_hz, 0.0,
        "the corpus tick must be derived"
    );

    let trace = engine
        .run(&scenario)
        .unwrap_or_else(|error| panic!("{label} whole-project run failed: {error}"));
    assert!(
        trace.time.len() >= 2,
        "{label}: smoke produced too few ticks"
    );
    assert!(
        !trace.channels.is_empty(),
        "{label}: whole-project run produced no channel columns"
    );
    for (channel, column) in &trace.channels {
        assert_eq!(
            column.len(),
            trace.time.len(),
            "{label}: channel {channel:?} is not dense over the tick grid"
        );
    }

    let base_rate = 1.0 / (trace.time[1] - trace.time[0]);
    for rate in scheduled_rates {
        let divisor = base_rate / rate;
        assert!(
            (divisor - divisor.round()).abs() < 1e-9,
            "{label}: derived {base_rate} Hz base cannot represent {rate} Hz"
        );
    }

    for call in ["System.FlashSize", "System.FlashFree"] {
        assert!(
            trace.hardware.iter().any(|item| {
                item.canonical_call() == call
                    && item.source == HardwareValueSource::ScenarioWildcard
            }),
            "{label}: {call} did not use explicit scenario metadata"
        );
    }

    let bus_receives = trace
        .hardware
        .iter()
        .filter(|item| item.source == HardwareValueSource::VirtualCanRx)
        .collect::<Vec<_>>();
    assert!(
        !bus_receives.is_empty(),
        "{label}: the virtual bus supplied no receive calls"
    );
    eprintln!(
        "{label}: {} virtual-CAN receive operation(s)",
        bus_receives.len()
    );
    for item in bus_receives {
        eprintln!(
            "  {} at {}:{}",
            item.canonical_call(),
            item.site.script(),
            item.site.offset()
        );
    }
    assert!(
        trace
            .hardware
            .iter()
            .any(|item| item.source == HardwareValueSource::VirtualCan),
        "{label}: the run exercised no deterministic virtual-CAN setup or transmit call"
    );
    assert!(
        trace.can.iter().any(|event| {
            event.direction == m1_eval::CanTransferDirection::Rx
                && event.bytes.iter().all(|byte| *byte == 0)
        }),
        "{label}: no zero-filled scenario CAN frame was consumed"
    );

    assert!(
        !trace.defaulted.is_empty(),
        "{label}: expected the offline smoke to disclose substituted inputs"
    );
    eprintln!(
        "{label}: {} substituted ordinary input(s)",
        trace.defaulted.len()
    );
    for (channel, substitution) in &trace.defaulted {
        assert!(
            !channel.is_empty(),
            "{label}: empty substituted channel name"
        );
        assert!(
            !substitution.first_reader.is_empty(),
            "{label}: {channel} has no first-reader identity"
        );
        eprintln!(
            "  {channel} = {:?}, first read by {}",
            substitution.value, substitution.first_reader
        );
    }

    trace
}

/// One zero-filled, time-zero frame for every receive-capable message whose bus
/// binding is concrete in the same loaded DBC snapshot. This remains entirely
/// read-only with respect to the proprietary corpus and commits no corpus data.
fn zero_can_frames(loaded: &Loaded) -> Vec<CanRx> {
    loaded
        .can
        .modules
        .iter()
        .flat_map(|module| {
            let bus = module.bus_value.and_then(|value| i32::try_from(value).ok());
            module.messages.iter().filter_map(move |message| {
                let bus = bus?;
                if message.direction == Some(CanDirection::Tx) {
                    return None;
                }
                Some(CanRx {
                    time_s: 0.0,
                    bus,
                    id: message.frame_id,
                    extended: message.format == CanFrameFormat::Extended,
                    bytes: vec![0; usize::from(message.dlc)],
                })
            })
        })
        .collect()
}

/// Supply a neutral, type-correct one for a configured zero parameter only
/// when that parameter is proven to make a statically evaluable division
/// denominator non-zero. Some checked-in projects deliberately leave optional
/// physical models untuned (all-zero coefficients). The compatibility smoke is
/// not a calibration-validity oracle, so it reports these scenario inputs while
/// normal evaluator runs retain their fail-loud divide/table behavior.
fn neutralize_zero_calibration_denominators(loaded: &Loaded) -> Vec<InputSeries> {
    let mut seeds = BTreeMap::<String, Value>::new();
    for script in &loaded.scripts {
        let group = loaded.project.group_for_script(&script.name);
        let fn_symbol = loaded.project.function_symbol_for_script(&script.name);
        let candidates = io_sets(script, &loaded.project, group.as_deref())
            .reads
            .into_iter()
            .filter_map(|path| {
                let symbol = loaded.project.symbols().get(&path)?;
                if symbol.kind != SymbolKind::Parameter {
                    return None;
                }
                let scalar = loaded.calib.param(&path).or_else(|| {
                    path.strip_prefix("Root.")
                        .and_then(|path| loaded.calib.param(path))
                })?;
                if scalar.as_f64() != 0.0 {
                    return None;
                }
                safe_nonzero_parameter(symbol).map(|value| (path, value))
            })
            .collect::<Vec<_>>();
        neutralize_script_denominators(
            loaded,
            script,
            group.as_deref(),
            fn_symbol.as_deref(),
            &script.cst.root(),
            &candidates,
            &mut seeds,
        );
    }
    seeds
        .into_iter()
        .map(|(channel, value)| InputSeries {
            channel,
            kind: InputKind::Const(value),
        })
        .collect()
}

fn safe_nonzero_parameter(symbol: &Symbol) -> Option<Value> {
    if symbol.declared_type.as_deref().is_some_and(|declared| {
        declared.eq_ignore_ascii_case("FixedPoint7dps")
            || declared.eq_ignore_ascii_case("fixed7dps")
    }) {
        return Some(Value::M1(M1Scalar::FixedPoint7dps(
            FixedPoint7dps::from_raw(FixedPoint7dps::SCALE as i32),
        )));
    }
    match symbol.value_type {
        ValueType::Integer => Some(Value::m1_integer(1)),
        ValueType::Unsigned => Some(Value::m1_unsigned(1)),
        ValueType::Float => Some(Value::m1_float(1.0)),
        ValueType::Boolean | ValueType::String | ValueType::Enum(_) | ValueType::Unknown => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn neutralize_script_denominators(
    loaded: &Loaded,
    script: &ParsedScript,
    group: Option<&str>,
    fn_symbol: Option<&str>,
    node: &Node,
    candidates: &[(String, Value)],
    seeds: &mut BTreeMap<String, Value>,
) {
    if node.kind() == Kind::BinaryExpression
        && node
            .child_by_field(Field::Operator)
            .is_some_and(|operator| operator.kind() == Kind::Slash)
        && let Some(denominator) = node.child_by_field(Field::Right)
        && evaluate_smoke_expression(loaded, script, group, fn_symbol, &denominator, seeds)
            .is_some_and(|value| value.m1_scalar().is_ok_and(|scalar| scalar.as_f64() == 0.0))
    {
        for (path, probe) in candidates {
            if seeds.contains_key(path) {
                continue;
            }
            let mut probed = seeds.clone();
            probed.insert(path.clone(), probe.clone());
            if evaluate_smoke_expression(loaded, script, group, fn_symbol, &denominator, &probed)
                .is_some_and(|value| {
                    value.m1_scalar().is_ok_and(|scalar| {
                        let scalar = scalar.as_f64();
                        scalar.is_finite() && scalar != 0.0
                    })
                })
            {
                seeds.insert(path.clone(), probe.clone());
                break;
            }
        }
    }
    for child in node.named_children() {
        neutralize_script_denominators(loaded, script, group, fn_symbol, &child, candidates, seeds);
    }
}

fn evaluate_smoke_expression(
    loaded: &Loaded,
    script: &ParsedScript,
    group: Option<&str>,
    fn_symbol: Option<&str>,
    node: &Node,
    seeds: &BTreeMap<String, Value>,
) -> Option<Value> {
    let mut env = Env::new();
    for (path, value) in seeds {
        env.set(path.clone(), value.clone());
    }
    let mut state = StateStore::new();
    let mut ctx = EvalCtx {
        project: &loaded.project,
        calib: &loaded.calib,
        env: &mut env,
        state: &mut state,
        group,
        fn_symbol,
        script_name: &script.name,
        dt: 0.001,
        scripts: &loaded.scripts,
        signature_m1_types: Some(&loaded.signature_m1_types),
        object_rules: Some(&loaded.object_rules),
        depth: 0,
        trace: None,
    };
    eval(node, &mut ctx).ok()
}

#[test]
fn corpus_discovery_finds_versioned_project_and_root_config() {
    let temp = tempfile::tempdir().expect("temporary corpus root");
    let version = temp.path().join("UQR-Test/01.00");
    std::fs::create_dir_all(&version).expect("create synthetic version directory");
    let project = version.join("Project.m1prj");
    let config = temp.path().join("parameters.m1cfg");
    std::fs::write(&project, "synthetic project marker").expect("write synthetic project marker");
    std::fs::write(&config, "synthetic config marker").expect("write synthetic config marker");

    for entry in [temp.path(), version.as_path(), project.as_path()] {
        let layout = discover_corpus(entry).expect("synthetic layout is discovered");
        assert_eq!(layout.project, project);
        assert_eq!(layout.config, config);
    }
}

#[test]
fn zero_denominator_neutralization_is_structural_and_minimal() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
    let loaded = load(
        &dir.join("Project.m1prj"),
        Some(&dir.join("parameters.m1cfg")),
    )
    .expect("configured enum fixture loads");
    let inputs = neutralize_zero_calibration_denominators(&loaded);
    assert_eq!(
        inputs.len(),
        1,
        "one side of a zero sum is enough to make the denominator non-zero"
    );
    assert!(
        matches!(inputs[0].kind, InputKind::Const(Value::M1(_))),
        "the synthetic float calibration gets a typed numeric scenario value"
    );
    assert!(
        inputs[0].channel.ends_with("Coefficient"),
        "the helper derives a parameter from the denominator instead of a corpus path"
    );
}
