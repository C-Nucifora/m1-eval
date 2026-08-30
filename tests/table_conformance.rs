// SPDX-License-Identifier: GPL-3.0-or-later
//! Table-specific golden vectors and optional private evidence gates.

use m1_eval::table::{lookup_inputs, lookup_values, validate};
use m1_eval::{
    CalAxisValues, ConformanceOptions, EvalError, ProvenanceKind, TableInput, Value, load,
    run_conformance_fixture, run_conformance_suite,
};
use std::path::{Path, PathBuf};

fn synthetic_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance/synthetic-tables.toml")
}

#[test]
fn synthetic_vectors_cover_table_layout_interpolation_and_boundaries() {
    let report = run_conformance_fixture(&synthetic_fixture())
        .expect("synthetic table conformance fixture passes");
    assert_eq!(report.provenance, ProvenanceKind::Synthetic);
    assert_eq!(report.steps_checked, 14);
    assert_eq!(report.assertions_checked, 84);
}

#[test]
fn enum_table_axis_rejects_an_unrelated_enum_with_the_same_declared_value() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/table-conformance");
    let loaded = load(
        &root.join("Project.m1prj"),
        Some(&root.join("parameters.m1cfg")),
    )
    .expect("synthetic table project loads");
    let expected_id = loaded
        .project
        .symbols()
        .enum_by_name("Table Mode")
        .expect("table enum type exists");
    let other_id = loaded
        .project
        .symbols()
        .enum_by_name("Other Table Mode")
        .expect("unrelated enum type exists");
    let table = loaded
        .calib
        .table("Tables.Enum Map")
        .expect("enum map calibration exists");
    assert!(matches!(
        &table.axes[0].values,
        CalAxisValues::Enum {
            enum_id: Some(id),
            ..
        } if *id == expected_id
    ));

    let error = lookup_values(
        table,
        &[Value::Enum {
            id: other_id,
            member: "Idle".to_string(),
        }],
        &loaded.project,
    )
    .expect_err("a numerically identical member from another enum must fail");
    assert!(matches!(error, EvalError::TypeError { .. }));
}

#[test]
fn configured_private_table_captures_form_an_optional_gate() {
    let Some(paths) = std::env::var_os("M1_EVAL_TABLE_M1_SIM_FIXTURES") else {
        return;
    };
    let paths: Vec<PathBuf> = std::env::split_paths(&paths).collect();
    assert!(
        !paths.is_empty(),
        "M1_EVAL_TABLE_M1_SIM_FIXTURES was set but contained no paths"
    );
    run_conformance_suite(
        &paths,
        ConformanceOptions {
            require_m1_sim_capture: true,
        },
    )
    .expect("configured M1 Sim table captures pass conformance");
}

#[test]
fn configured_real_project_tables_are_deterministic() {
    let project = std::env::var_os("M1_EVAL_TABLE_PROJECT").map(PathBuf::from);
    let config = std::env::var_os("M1_EVAL_TABLE_CONFIG").map(PathBuf::from);
    let (project, config) = match (project, config) {
        (None, None) => return,
        (Some(project), Some(config)) => (project, config),
        _ => panic!("set M1_EVAL_TABLE_PROJECT and M1_EVAL_TABLE_CONFIG together"),
    };

    let loaded = load(&project, Some(&config)).expect("configured real project and config load");
    let mut table_names: Vec<&str> = loaded.calib.tables.keys().map(String::as_str).collect();
    table_names.sort_unstable();
    assert!(!table_names.is_empty(), "configured project has no tables");

    let mut evaluated = 0usize;
    for name in table_names {
        let table = loaded.calib.table(name).expect("table key remains present");
        match validate(table) {
            Ok(()) => {
                let first_inputs: Vec<TableInput> = table
                    .axes
                    .iter()
                    .map(|axis| match &axis.values {
                        CalAxisValues::Numeric(values) => TableInput::Numeric(values[0]),
                        CalAxisValues::Enum {
                            values,
                            enum_id: Some(enum_id),
                        } => TableInput::Enum {
                            enum_id: *enum_id,
                            value: values[0],
                        },
                        CalAxisValues::Enum { enum_id: None, .. } => {
                            unreachable!("validated enum axis has a project enum id")
                        }
                    })
                    .collect();
                let first = lookup_inputs(table, &first_inputs)
                    .expect("validated table evaluates at its first sites");
                let second = lookup_inputs(table, &first_inputs)
                    .expect("repeated table evaluation succeeds");
                assert_eq!(
                    first, second,
                    "table {name:?} changed across identical calls"
                );
                evaluated += 1;
            }
            Err(first) => {
                let second = validate(table).expect_err("invalid table stays invalid");
                assert_eq!(
                    first.to_string(),
                    second.to_string(),
                    "table {name:?} produced an unstable diagnostic"
                );
            }
        }
    }
    assert!(
        evaluated > 0,
        "configured project has no valid table to evaluate"
    );
}
