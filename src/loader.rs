// SPDX-License-Identifier: GPL-3.0-or-later
//! Project + calibration loader.
//!
//! Mirrors the discovery/loading pattern in `m1-doc/src/loader.rs`: recursively
//! collect every `*.m1scr` under the project directory as `(basename, source)`
//! pairs, build the `m1-typecheck` [`Project`], infer user-function return types
//! from the script bodies, then read the calibration *values* from the optional
//! `.m1cfg`.
//!
//! The discovered scripts are parsed once via `m1_typecheck::parsed::parse_all`
//! into [`ParsedScript`]s (each owns its `Cst`); the evaluator walks these CSTs
//! every tick without reparsing. The loader is an internal seam, so it does
//! surface `Project` and the parsed CSTs to the rest of the crate — the public
//! `Engine` facade (a later task) re-wraps it so no toolchain types leak past the
//! library boundary.

use crate::builtins::object::ObjectRules;
use crate::calib::Calibration;
use crate::error::EvalError;
use crate::triggers::{TriggerMap, TriggerStatus};
use crate::value::M1ScalarKind;
use m1_can::{CanDbcSource, CanRuntimeModel, runtime_model_loaded};
use m1_typecheck::Project;
use m1_typecheck::parsed::{ParsedScript, parse_all};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Exact numeric storage families declared by project function signatures.
///
/// `m1-typecheck` intentionally exposes the language-level `ValueType` lattice,
/// which currently collapses `FixedPoint7dps` into `Unknown` in signatures and
/// does not retain whether a return type was declared or inferred. The evaluator
/// keeps this narrow companion index so call and return boundaries can obey the
/// raw project declaration without changing the upstream public model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureM1Types {
    return_kinds: HashMap<String, M1ScalarKind>,
    param_kinds: HashMap<(String, String), M1ScalarKind>,
}

impl SignatureM1Types {
    /// Exact declared numeric return family for `function`, if the signature
    /// declares one. Absence means the return is inferred or non-numeric.
    pub fn return_kind(&self, function: &str) -> Option<M1ScalarKind> {
        self.return_kinds.get(function).copied()
    }

    /// Exact declared numeric family for one named input parameter.
    pub fn param_kind(&self, function: &str, param: &str) -> Option<M1ScalarKind> {
        self.param_kinds
            .get(&(function.to_string(), param.to_string()))
            .copied()
    }
}

/// The result of loading a project: the typed symbol model, parsed scripts,
/// exact CAN model, and numeric calibration read from the `.m1cfg` (empty when
/// none given).
///
/// Adding [`Loaded::can`] is a source migration for callers that construct this
/// public struct directly. Prefer [`load`], which reads each `.m1dbc` once and
/// builds the field from the same project and script snapshot. A manual loader
/// can build the field with the re-exported
/// [`crate::runtime_model_loaded`] and [`crate::CanDbcSource`] API before using
/// its existing `Loaded` struct literal.
pub struct Loaded {
    /// The `m1-typecheck` project (symbols + resolution model).
    pub project: Project,
    /// Every discovered `*.m1scr`, parsed once (name + owned CST).
    pub scripts: Vec<ParsedScript>,
    /// Exact DBC layout and bus bindings built from the same project, parsed
    /// scripts, and caller-owned `.m1dbc` byte snapshot as this load.
    pub can: CanRuntimeModel,
    /// Numeric calibration values (parameters + table cells).
    pub calib: Calibration,
    /// Exact numeric families from raw function signatures. This complements
    /// the typechecker's coarser `ValueType` model at evaluator boundaries.
    pub signature_m1_types: SignatureM1Types,
    /// Validation ranges declared on project value objects. Core object methods
    /// use this index for `Validate`, `Constrain`, and `Set`.
    pub object_rules: ObjectRules,
    /// Function symbols whose `.m1prj` `SelectedTrigger` resolves to the
    /// `On Startup` event kernel, including any resolved attribute reference.
    /// The whole-project runner executes these exactly once before the periodic
    /// loop.
    pub startup_fn_symbols: Vec<String>,
    /// Effective trigger state for every script-backed function. Runtime and
    /// coverage both use this map instead of the lossy `call_rate_hz` field.
    pub triggers: TriggerMap,
}

/// Load a project, its scripts, and (optionally) its calibration values.
///
/// `project_path` points at the `.m1prj`; scripts are discovered by walking that
/// file's parent directory recursively. `cfg_path`, when given, is loaded twice:
/// once into the `Project` (via `with_config`, for table/parameter *shape* and
/// types) and once into our [`Calibration`] value reader (for the actual
/// numbers).
///
/// Fails loud: any `m1-typecheck` `LoadError` or `.m1cfg` read/parse error is
/// mapped onto an [`EvalError`] rather than swallowed.
pub fn load(project_path: &Path, cfg_path: Option<&Path>) -> Result<Loaded, EvalError> {
    let mut project = Project::load(project_path).map_err(load_err)?;

    // Augment the project with the cfg's table/parameter shape if provided. This
    // is the `m1-typecheck` view; the numeric values come from our own reader
    // below.
    if let Some(cfg) = cfg_path {
        project = project.with_config(cfg).map_err(load_err)?;
    }

    // Discover scripts relative to the project file's directory (mirrors m1-doc).
    let project_dir = project_path.parent().unwrap_or_else(|| Path::new("."));
    let pairs = collect_scripts(project_dir);

    // Parse each discovered script exactly once; the CSTs are shared with the
    // return-type inference pass and reused by the evaluator each tick.
    let scripts = parse_all(&pairs);

    // Infer user-function return types from the script bodies before the
    // evaluator runs, so call sites and `Out =` reads see concrete types.
    project.infer_return_types(&scripts);

    // Read every DBC source exactly once, retain its project-relative identity,
    // and build the runtime model from this same Project + ParsedScript
    // snapshot. m1-can performs no filesystem I/O on this path.
    let can_source_bytes = collect_can_sources(project_dir)?;
    let can_sources = can_source_bytes
        .iter()
        .map(|source| CanDbcSource {
            path: &source.path,
            bytes: &source.bytes,
        })
        .collect::<Vec<_>>();
    let can = runtime_model_loaded(&project, &scripts, &can_sources).map_err(|error| {
        EvalError::UnsupportedConstruct {
            kind: format!("CAN runtime model load failed: {error}"),
            at: 0,
        }
    })?;

    // Read the calibration *values*. We read the file ourselves (rather than
    // re-using `m1-typecheck`'s loader) because `with_config` keeps only the
    // shape; `Calibration::from_m1cfg_str` keeps the numbers.
    let mut calib = match cfg_path {
        Some(cfg) => {
            let xml = read_xml(cfg)?;
            Calibration::from_m1cfg_str(&xml)?
        }
        None => Calibration::default(),
    };

    let project_xml = read_xml(project_path)?;
    calib.apply_project_table_properties(&project_xml, &project)?;
    let signature_m1_types = signature_m1_types(&project_xml)?;
    let object_rules = ObjectRules::from_project_xml(&project_xml)?;
    let triggers = TriggerMap::from_project_xml(&project_xml, &project, &scripts)?;
    let startup_fn_symbols = triggers
        .iter()
        .filter_map(|(function, status)| {
            matches!(status, TriggerStatus::Startup).then_some(function.to_string())
        })
        .collect();

    Ok(Loaded {
        project,
        scripts,
        can,
        calib,
        signature_m1_types,
        object_rules,
        startup_fn_symbols,
        triggers,
    })
}

struct LoadedCanSource {
    path: String,
    bytes: Vec<u8>,
}

fn collect_can_sources(project_dir: &Path) -> Result<Vec<LoadedCanSource>, EvalError> {
    let mut paths = Vec::new();
    collect_extension_paths(project_dir, "m1dbc", &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let relative = path.strip_prefix(project_dir).map_err(|error| {
                EvalError::UnsupportedConstruct {
                    kind: format!(
                        "CAN source {} is outside project directory {}: {error}",
                        path.display(),
                        project_dir.display()
                    ),
                    at: 0,
                }
            })?;
            let source_path = relative
                .to_str()
                .ok_or_else(|| EvalError::UnsupportedConstruct {
                    kind: format!(
                        "CAN source path {} is not valid UTF-8 and cannot be preserved exactly",
                        relative.display()
                    ),
                    at: 0,
                })?;
            let bytes = std::fs::read(&path).map_err(|error| EvalError::UnsupportedConstruct {
                kind: format!("cannot read CAN source {}: {error}", path.display()),
                at: 0,
            })?;
            Ok(LoadedCanSource {
                path: source_path.to_string(),
                bytes,
            })
        })
        .collect()
}

fn collect_extension_paths(
    dir: &Path,
    extension: &str,
    out: &mut Vec<PathBuf>,
) -> Result<(), EvalError> {
    let entries = std::fs::read_dir(dir).map_err(|error| EvalError::UnsupportedConstruct {
        kind: format!("cannot read project directory {}: {error}", dir.display()),
        at: 0,
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| EvalError::UnsupportedConstruct {
            kind: format!(
                "cannot enumerate project directory {}: {error}",
                dir.display()
            ),
            at: 0,
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| EvalError::UnsupportedConstruct {
                kind: format!("cannot inspect project path {}: {error}", path.display()),
                at: 0,
            })?;
        if file_type.is_dir() {
            collect_extension_paths(&path, extension, out)?;
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn signature_m1_types(xml: &str) -> Result<SignatureM1Types, EvalError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| EvalError::UnsupportedConstruct {
        kind: format!("project XML re-parse for signature types failed: {e}"),
        at: 0,
    })?;
    let mut types = SignatureM1Types::default();
    for component in doc
        .descendants()
        .filter(|node| node.has_tag_name("Component"))
    {
        let Some(function) = component.attribute("Name") else {
            continue;
        };
        let Some(signature) = component
            .children()
            .find(|node| node.has_tag_name("Signature"))
        else {
            continue;
        };
        if let Some(kind) = signature
            .attribute("ReturnType")
            .and_then(signature_scalar_kind)
        {
            types.return_kinds.insert(function.to_string(), kind);
        }
        if let Some(params) = signature
            .children()
            .find(|node| node.has_tag_name("Params"))
        {
            for param in params.children().filter(|node| node.has_tag_name("Param")) {
                let (Some(name), Some(kind)) = (
                    param.attribute("Name"),
                    param.attribute("Type").and_then(signature_scalar_kind),
                ) else {
                    continue;
                };
                types
                    .param_kinds
                    .insert((function.to_string(), name.to_string()), kind);
            }
        }
    }
    Ok(types)
}

fn signature_scalar_kind(raw: &str) -> Option<M1ScalarKind> {
    match raw.to_ascii_lowercase().replace([' ', '_'], "").as_str() {
        "f32" | "f64" | "float" | "floatingpoint" => Some(M1ScalarKind::FloatingPoint),
        "s8" | "s16" | "s32" | "s64" | "integer" => Some(M1ScalarKind::Integer),
        "u8" | "u16" | "u32" | "u64" | "unsignedinteger" => Some(M1ScalarKind::UnsignedInteger),
        "fixedpoint7dps" | "fixed7dps" => Some(M1ScalarKind::FixedPoint7dps),
        _ => None,
    }
}
/// Read a MoTeC XML file as text, decoding lossily so Windows-1252 exports do not
/// abort the load. Maps IO failure onto a fail-loud [`EvalError`].
fn read_xml(path: &Path) -> Result<String, EvalError> {
    let bytes = std::fs::read(path).map_err(|e| EvalError::MissingCalibration {
        path: format!("{}: {e}", path.display()),
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Map a `m1-typecheck` `LoadError` onto our fail-loud [`EvalError`]. A project
/// that will not load is a hard error, not a recoverable condition.
fn load_err(e: m1_typecheck::project::LoadError) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: format!("project load failed: {e}"),
        at: 0,
    }
}

/// Collect every `.m1scr` under `dir` (recursively) as `(basename, source)`
/// pairs. Sources are lossy-UTF-8 decoded. Mirrors `m1-doc/src/loader.rs`.
fn collect_scripts(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect_scripts_rec(dir, &mut out);
    // Deterministic order: sort by basename so the tick loop and traces are
    // reproducible regardless of filesystem enumeration order.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect_scripts_rec(dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_scripts_rec(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("m1scr") {
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let bytes = std::fs::read(&path).unwrap_or_default();
            let source = String::from_utf8_lossy(&bytes).into_owned();
            out.push((name, source));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute path to the hand-authored synthetic `tests/fixtures/mini`
    /// project. Synthetic: no proprietary MoTeC content.
    fn mini_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini")
    }

    fn virtual_can_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/virtual_can")
    }

    #[test]
    fn loads_project_scripts_and_calibration() {
        let dir = mini_dir();
        let prj = dir.join("Project.m1prj");
        let cfg = dir.join("parameters.m1cfg");

        let loaded = load(&prj, Some(&cfg)).expect("mini fixture should load");

        // At least the project's declared symbols are present.
        assert!(
            loaded.project.symbols().iter().count() >= 1,
            "expected >=1 symbol"
        );
        // The FuncUser symbol backing the script resolves.
        assert!(
            loaded.project.symbols().get("Root.Demo.Update").is_some(),
            "Root.Demo.Update function symbol present"
        );

        // The one script was discovered and parsed.
        assert_eq!(loaded.scripts.len(), 1, "one .m1scr discovered");
        assert_eq!(loaded.scripts[0].name, "Demo.Update.m1scr");
        // The CST owns its non-empty source.
        assert!(!loaded.scripts[0].cst.source().is_empty());

        // The calibration value reader read the gain parameter. The `.m1cfg`
        // writes the unprefixed name `Demo.Gain` (real exports omit `Root.`).
        assert_eq!(
            loaded.calib.param("Demo.Gain"),
            Some(crate::value::M1Scalar::FloatingPoint(2.5))
        );
        // And the 2-D table cells.
        let map = loaded.calib.table("Demo.Map").expect("Demo.Map table");
        assert_eq!(map.axes.len(), 2);
        assert_eq!(
            map.body,
            vec![10.0, 30.0, 20.0, 40.0]
                .into_iter()
                .map(crate::value::M1Scalar::FloatingPoint)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn loads_without_config() {
        let dir = mini_dir();
        let prj = dir.join("Project.m1prj");

        let loaded = load(&prj, None).expect("load without cfg");
        assert_eq!(loaded.scripts.len(), 1);
        // No cfg means an empty calibration, not a guessed value.
        assert_eq!(loaded.calib.param("Demo.Gain"), None);
        assert!(loaded.calib.tables.is_empty());
    }

    #[test]
    fn loads_nested_dbc_from_the_same_owned_source_and_script_snapshot() {
        let source = virtual_can_dir();
        let temp = tempfile::tempdir().expect("temporary virtual-CAN fixture");
        let scripts = temp.path().join("Scripts");
        let dbc_dir = temp.path().join("dbc/vendor files");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&dbc_dir).unwrap();
        std::fs::copy(
            source.join("Project.m1prj"),
            temp.path().join("Project.m1prj"),
        )
        .unwrap();
        for script in ["CAN Receive.m1scr", "CAN Transmit.m1scr"] {
            std::fs::copy(source.join("Scripts").join(script), scripts.join(script)).unwrap();
        }
        let dbc = dbc_dir.join("Vehicle Network.m1dbc");
        std::fs::copy(source.join("dbc/vendor files/Vehicle Network.m1dbc"), &dbc).unwrap();

        let loaded = load(&temp.path().join("Project.m1prj"), None)
            .expect("copied virtual-CAN fixture loads");
        let module = &loaded.can.modules[0];
        assert_eq!(module.path, "Vehicle Network");
        assert_eq!(module.source_path, "dbc/vendor files/Vehicle Network.m1dbc");
        assert_eq!(module.bus_value, Some(0));
        assert_eq!(module.messages[0].frame_id, 0x123);

        // Changing both source files after load cannot alter the caller-owned
        // bytes, parsed scripts, or runtime model retained by `Loaded`.
        std::fs::write(&dbc, "<DBC><broken-after-load/></DBC>").unwrap();
        std::fs::write(scripts.join("CAN Receive.m1scr"), "broken after load").unwrap();
        assert_eq!(loaded.can.modules[0].bus_value, Some(0));
        assert_eq!(loaded.can.modules[0].messages[0].frame_id, 0x123);
        assert!(loaded.scripts.iter().any(|script| {
            script.name == "CAN Receive.m1scr"
                && script.cst.source().contains("Status Frame.Receive")
        }));
    }

    #[test]
    fn missing_project_fails_loud() {
        let missing = mini_dir().join("DoesNotExist.m1prj");
        match load(&missing, None) {
            Ok(_) => panic!("missing project should fail loud"),
            Err(e) => assert!(
                matches!(e, EvalError::UnsupportedConstruct { .. }),
                "missing project should fail loud, got {e:?}"
            ),
        }
    }
}
