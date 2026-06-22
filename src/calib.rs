// SPDX-License-Identifier: GPL-3.0-or-later
//! `.m1cfg` calibration VALUE reader.
//!
//! `m1-typecheck`'s `with_config` keeps only the *shape* of tables and the
//! *types/units* of parameters — it deliberately discards the actual numbers.
//! This module reads those numbers: scalar parameter values, table axis
//! breakpoints, and table body cells. Table lookup (`src/table.rs`) consumes
//! the [`CalTable`] values produced here.
//!
//! ## Real-file grounding
//!
//! The element/attribute names were confirmed against a real
//! `parameters.m1cfg` export and against `m1-typecheck`'s own parser
//! (`symbols/m1cfg.rs`, pinned commit):
//!
//! - The root element is `<Configuration>`, with `<Parameter>` entries nested
//!   under one or more `<Group>` elements. We match `<Parameter>`/`<Table>`
//!   anywhere via descendant traversal, so nesting depth is irrelevant.
//! - A `<Parameter Name="...">` holds a single `<Cell Type="..." Unit="...">`.
//!   Cell content may be a `<![CDATA[...]]>` block or plain text; `roxmltree`'s
//!   `Node::text()` returns the CDATA content either way.
//! - Numbers may be in scientific notation (e.g. `1.0000e-003`); `f64::from_str`
//!   handles that.
//! - `enum` cells carry a non-numeric member name (e.g. `On`). They are not
//!   numeric calibration values, so they are skipped here (and surface as a
//!   `MissingCalibration` if some script later reads them as a number).
//! - A `<Table Name="...">` has ordered `<X>`/`<Y>`/`<Z>` axis children, each
//!   wrapping a `<Cells>` of breakpoint `<Cell>`s, plus a `<Body><Cells>` of
//!   interpolation values.
//!
//! Names are stored verbatim as the `.m1cfg` writes them. Real exports omit the
//! implicit `Root.` group prefix that the symbol table uses; canonicalisation
//! to symbol paths is the caller's concern (see the loader / lookup wiring),
//! kept out of this pure reader.

use crate::error::EvalError;
use std::collections::HashMap;

/// A calibration table's concrete numbers: one breakpoint vector per input
/// axis (in `<X>`,`<Y>`,`<Z>` order) plus the flat body cells.
///
/// ## Body memory layout
///
/// `body` is row-major with **axis 0 (X) outermost**: for a 2-D table the cell
/// for breakpoint indices `(ix, iy)` lives at `ix * ny + iy` where `ny =
/// axes[1].len()`. Generally the stride of axis `k` is the product of the
/// lengths of all axes after `k`. `src/table.rs` relies on this layout.
#[derive(Debug, Clone, PartialEq)]
pub struct CalTable {
    /// Breakpoint values per input axis, outermost first.
    pub axes: Vec<Vec<f64>>,
    /// Flat body cells, row-major with axis 0 outermost.
    pub body: Vec<f64>,
}

/// All numeric calibration values read from a `.m1cfg`: scalar parameters and
/// tables, keyed by the name written in the file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Calibration {
    /// Scalar parameter values, keyed by `<Parameter Name>`.
    pub params: HashMap<String, f64>,
    /// Table values, keyed by `<Table Name>`.
    pub tables: HashMap<String, CalTable>,
}

impl Calibration {
    /// Parse a `.m1cfg` document's numeric values from its XML text.
    ///
    /// Fails loud only on malformed XML; individual non-numeric cells (e.g.
    /// `enum` members) are skipped rather than guessed.
    pub fn from_m1cfg_str(xml: &str) -> Result<Calibration, EvalError> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| EvalError::MissingCalibration {
            path: format!(".m1cfg parse error: {e}"),
        })?;

        let mut params = HashMap::new();
        for param in doc.descendants().filter(|n| n.has_tag_name("Parameter")) {
            let Some(name) = param.attribute("Name") else {
                continue;
            };
            let Some(cell) = param.children().find(|c| c.has_tag_name("Cell")) else {
                continue;
            };
            // Skip non-numeric cells (enum members etc.) — they are not
            // calibration *numbers*. A script that later reads such a value as
            // a number will fail loud at that read, not here.
            if let Some(v) = cell_value(cell) {
                params.insert(name.to_string(), v);
            }
        }

        let mut tables = HashMap::new();
        for tbl in doc.descendants().filter(|n| n.has_tag_name("Table")) {
            let Some(name) = tbl.attribute("Name") else {
                continue;
            };
            tables.insert(name.to_string(), parse_table(tbl)?);
        }

        Ok(Calibration { params, tables })
    }

    /// The scalar value of a parameter, if the `.m1cfg` provided a numeric one.
    pub fn param(&self, path: &str) -> Option<f64> {
        self.params.get(path).copied()
    }

    /// The table values for a table path, if present.
    pub fn table(&self, path: &str) -> Option<&CalTable> {
        self.tables.get(path)
    }
}

/// Parse a single `<Cell>` element's text as an `f64`. Returns `None` for
/// empty or non-numeric content (e.g. an enum member name).
fn cell_value(cell: roxmltree::Node<'_, '_>) -> Option<f64> {
    let text = cell.text()?.trim();
    text.parse::<f64>().ok()
}

/// Collect the `<Cell>` breakpoint/body values under a `<Cells>` wrapper found
/// directly inside `parent` (an axis `<X>/<Y>/<Z>` or a `<Body>`). Non-numeric
/// cells fail loud, since a table cannot be silently missing interpolation
/// data.
fn cells_values(parent: roxmltree::Node<'_, '_>, ctx: &str) -> Result<Vec<f64>, EvalError> {
    let Some(cells) = parent.children().find(|c| c.has_tag_name("Cells")) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for cell in cells.children().filter(|c| c.has_tag_name("Cell")) {
        match cell_value(cell) {
            Some(v) => out.push(v),
            None => {
                return Err(EvalError::MissingCalibration {
                    path: format!("non-numeric table cell in {ctx}"),
                });
            }
        }
    }
    Ok(out)
}

/// Parse a `<Table>` element into its concrete [`CalTable`]: ordered `<X>`,
/// `<Y>`, `<Z>` axis breakpoints and the `<Body>` cells.
fn parse_table(tbl: roxmltree::Node<'_, '_>) -> Result<CalTable, EvalError> {
    let name = tbl.attribute("Name").unwrap_or("<unnamed>");
    let mut axes = Vec::new();
    for tag in ["X", "Y", "Z"] {
        if let Some(axis) = tbl.children().find(|c| c.has_tag_name(tag)) {
            axes.push(cells_values(axis, &format!("table {name} axis {tag}"))?);
        }
    }
    let body = match tbl.children().find(|c| c.has_tag_name("Body")) {
        Some(b) => cells_values(b, &format!("table {name} body"))?,
        None => Vec::new(),
    };
    Ok(CalTable { axes, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic 2-D table + scalar, mirroring the m1cfg fixture shape:
    /// `<Configuration>` root with `<Parameter>`/`<Table>` entries.
    const XML: &str = r#"<Configuration>
      <Parameter Name="Root.A.Gain"><Cell Type="f32">2.5</Cell></Parameter>
      <Table Name="Root.A.Map">
        <X><Cells Type="f32" Unit="rpm"><Cell>0</Cell><Cell>100</Cell></Cells></X>
        <Y><Cells Type="f32" Unit="%"><Cell>0</Cell><Cell>1</Cell></Cells></Y>
        <Body><Cells Type="f32"><Cell>10</Cell><Cell>20</Cell><Cell>30</Cell><Cell>40</Cell></Cells></Body>
      </Table>
    </Configuration>"#;

    #[test]
    fn reads_param_and_table() {
        let c = Calibration::from_m1cfg_str(XML).unwrap();
        assert_eq!(c.param("Root.A.Gain"), Some(2.5));
        let t = c.table("Root.A.Map").unwrap();
        assert_eq!(t.axes.len(), 2);
        assert_eq!(t.axes[0], vec![0.0, 100.0]);
        assert_eq!(t.axes[1], vec![0.0, 1.0]);
        assert_eq!(t.body, vec![10.0, 20.0, 30.0, 40.0]);
    }

    /// A trimmed, *synthetic* approximation of the real export shape: root
    /// `<Configuration>`, a nested `<Group>`, CDATA cell bodies, scientific
    /// notation, an `enum` cell that must be skipped (not numeric), and the
    /// unprefixed names real MoTeC exports use.
    const REAL_SHAPE_XML: &str = r#"<?xml version="1.0"?>
<Configuration Locale="English_Australia.1252" DefaultLocale="C">
 <Group Name="">
  <Parameter Name="Outputs.Logging.LVLogging">
   <Cell Type="enum">
<![CDATA[On]]>
   </Cell>
  </Parameter>
  <Parameter Name="Inputs.APPS.APPS1.Offset">
   <Cell Type="f32" Unit="V">
<![CDATA[3.67013192176818850e+000]]>
   </Cell>
  </Parameter>
  <Parameter Name="Inputs.APPS.CompareThreshold">
   <Cell Type="f32">
<![CDATA[2.00000002980232240e-001]]>
   </Cell>
  </Parameter>
 </Group>
</Configuration>"#;

    #[test]
    fn reads_real_export_shape() {
        let c = Calibration::from_m1cfg_str(REAL_SHAPE_XML).unwrap();
        // CDATA + scientific notation parse to f64.
        assert!((c.param("Inputs.APPS.APPS1.Offset").unwrap() - 3.6701319217681885).abs() < 1e-9);
        assert!(
            (c.param("Inputs.APPS.CompareThreshold").unwrap() - 0.20000000298023224).abs() < 1e-12
        );
        // The enum cell is not a numeric calibration value: skipped, not guessed.
        assert_eq!(c.param("Outputs.Logging.LVLogging"), None);
    }

    #[test]
    fn malformed_xml_fails_loud() {
        let err = Calibration::from_m1cfg_str("<Configuration><Parameter>").unwrap_err();
        assert!(matches!(err, EvalError::MissingCalibration { .. }));
    }

    #[test]
    fn empty_config_is_empty_calibration() {
        let c = Calibration::from_m1cfg_str("<Configuration/>").unwrap();
        assert!(c.params.is_empty());
        assert!(c.tables.is_empty());
        assert_eq!(c.param("anything"), None);
        assert!(c.table("anything").is_none());
    }

    #[test]
    fn one_dimensional_table() {
        let xml = r#"<Configuration>
          <Table Name="Root.Curve">
            <X><Cells Type="f32"><Cell>0</Cell><Cell>1</Cell><Cell>2</Cell></Cells></X>
            <Body><Cells Type="f32"><Cell>5</Cell><Cell>15</Cell><Cell>25</Cell></Cells></Body>
          </Table>
        </Configuration>"#;
        let c = Calibration::from_m1cfg_str(xml).unwrap();
        let t = c.table("Root.Curve").unwrap();
        assert_eq!(t.axes.len(), 1);
        assert_eq!(t.axes[0], vec![0.0, 1.0, 2.0]);
        assert_eq!(t.body, vec![5.0, 15.0, 25.0]);
    }
}
