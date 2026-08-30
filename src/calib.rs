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
//! - Numbers may be in scientific notation (e.g. `1.0000e-003`), and unsigned
//!   cells may use M1 hexadecimal notation (e.g. `0x400`). They are parsed
//!   according to the cell's declared type and restored to their M1-width scalar
//!   family. Untyped and historical `f64` cells narrow to M1 binary32.
//! - Scalar `enum` cells usually carry a member name (e.g. `On`) and are skipped.
//!   Table enum axes instead contain exact declared numeric integer values and
//!   retain their enum identity. Boolean cells are not numeric calibration
//!   values and are skipped.
//! - A `<Table Name="...">` has ordered `<X>`/`<Y>`/`<Z>` axis children, each
//!   wrapping a `<Cells>` of breakpoint `<Cell>`s, plus a `<Body><Cells>` of
//!   interpolation values. `Site="x,y,z"` attributes define the axis and body
//!   order independently of XML element order.
//!
//! Names are stored verbatim as the `.m1cfg` writes them. Real exports omit the
//! implicit `Root.` group prefix that the symbol table uses; canonicalisation
//! to symbol paths is the caller's concern (see the loader / lookup wiring),
//! kept out of this pure reader.

use crate::error::EvalError;
use crate::value::{FixedPoint7dps, M1Scalar};
use m1_typecheck::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::{EnumId, SymbolKind};
use std::collections::HashMap;

/// Boundary policy for one numeric table axis.
///
/// M1 project metadata records this on the table's `<Axis>` entry. Missing
/// metadata means that both ends clamp. Enum axes always use [`Self::Clamp`]
/// because their sites are categorical, not interpolated coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AxisExtrapolation {
    /// Clamp below the first site and above the last site.
    #[default]
    Clamp,
    /// Extrapolate below the first site and clamp above the last site.
    Below,
    /// Clamp below the first site and extrapolate above the last site.
    Above,
    /// Extrapolate at both ends.
    Both,
}

impl AxisExtrapolation {
    pub(crate) const fn below(self) -> bool {
        matches!(self, Self::Below | Self::Both)
    }

    pub(crate) const fn above(self) -> bool {
        matches!(self, Self::Above | Self::Both)
    }
}

/// Calibrated sites for one table axis.
#[derive(Debug, Clone, PartialEq)]
pub enum CalAxisValues {
    /// Ordered numeric breakpoints. Values retain their declared M1 family.
    Numeric(Vec<M1Scalar>),
    /// Ordered declared enum values and the project enum type that owns them.
    /// Enum axes select a site exactly and never interpolate between members.
    Enum {
        /// Declared integer value at each calibrated site.
        values: Vec<i64>,
        /// Enum type resolved from the project axis `Source`. A raw `.m1cfg`
        /// parse leaves this unbound until project properties are applied.
        enum_id: Option<EnumId>,
    },
}

/// One table axis and its boundary policy.
#[derive(Debug, Clone, PartialEq)]
pub struct CalAxis {
    /// Ordered numeric breakpoints or declared enum values.
    pub values: CalAxisValues,
    /// Numeric boundary behavior read from the project descriptor.
    pub extrapolation: AxisExtrapolation,
}

impl CalAxis {
    /// Construct a clamped numeric axis.
    pub fn numeric(values: Vec<M1Scalar>) -> Self {
        Self {
            values: CalAxisValues::Numeric(values),
            extrapolation: AxisExtrapolation::Clamp,
        }
    }

    /// Construct a categorical enum axis.
    pub fn enumerated(values: Vec<i64>) -> Self {
        Self {
            values: CalAxisValues::Enum {
                values,
                enum_id: None,
            },
            extrapolation: AxisExtrapolation::Clamp,
        }
    }

    /// Construct a categorical enum axis bound to its project enum type.
    pub fn enumerated_for(enum_id: EnumId, values: Vec<i64>) -> Self {
        Self {
            values: CalAxisValues::Enum {
                values,
                enum_id: Some(enum_id),
            },
            extrapolation: AxisExtrapolation::Clamp,
        }
    }

    /// Number of calibrated sites on this axis.
    pub fn len(&self) -> usize {
        match &self.values {
            CalAxisValues::Numeric(values) => values.len(),
            CalAxisValues::Enum { values, .. } => values.len(),
        }
    }

    /// Whether this axis has no calibrated sites.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A calibration table's concrete numbers: one breakpoint vector per input
/// axis (in `<X>`,`<Y>`,`<Z>` order) plus the flat body cells.
///
/// ## Body memory layout
///
/// `body` uses the M1 site order recorded by `.m1cfg` `Site="x,y,z"`
/// coordinates: X changes fastest, then Y, then Z. For a 2-D table the cell at
/// `(ix, iy)` lives at `ix + nx * iy`, where `nx = axes[0].len()`.
#[derive(Debug, Clone, PartialEq)]
pub struct CalTable {
    /// Axis sites in X, Y, Z order.
    pub axes: Vec<CalAxis>,
    /// Flat body cells in X-fastest M1 site order.
    pub body: Vec<M1Scalar>,
}

impl CalTable {
    /// Construct a table with clamped numeric axes.
    pub fn numeric(axes: Vec<Vec<M1Scalar>>, body: Vec<M1Scalar>) -> Self {
        Self {
            axes: axes.into_iter().map(CalAxis::numeric).collect(),
            body,
        }
    }
}

/// Calibration values read from a `.m1cfg`: numeric scalar parameters and
/// numeric or enum-axis tables, keyed by the name written in the file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Calibration {
    /// Scalar parameter values, keyed by `<Parameter Name>`.
    pub params: HashMap<String, M1Scalar>,
    /// Table values, keyed by `<Table Name>`.
    pub tables: HashMap<String, CalTable>,
}

impl Calibration {
    /// Parse a `.m1cfg` document's numeric values from its XML text.
    ///
    /// Malformed XML, unsupported numeric types, and values outside their M1
    /// storage range fail loud. Named enum members and Boolean parameters are
    /// skipped; numeric enum representations and enum table axes are retained.
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
            // Skip named enum members and booleans — they are not calibration
            // numbers. Numeric enum representations remain available to the
            // table reader and to unusual scalar exports that use that form.
            if let Some(v) = cell_value(cell, cell.attribute("Type"), name)? {
                params.insert(name.to_string(), v);
            }
        }

        let mut tables = HashMap::new();
        for tbl in doc.descendants().filter(|n| n.has_tag_name("Table")) {
            let Some(name) = tbl.attribute("Name") else {
                continue;
            };
            let table = parse_table(tbl)?;
            if tables.insert(name.to_string(), table).is_some() {
                return Err(EvalError::MissingCalibration {
                    path: format!("calibration declares table {name:?} more than once"),
                });
            }
        }

        Ok(Calibration { params, tables })
    }

    /// The scalar value of a parameter, if the `.m1cfg` provided a numeric one.
    pub fn param(&self, path: &str) -> Option<M1Scalar> {
        self.params.get(path).copied()
    }

    /// The table values for a table path, if present.
    pub fn table(&self, path: &str) -> Option<&CalTable> {
        self.tables.get(path)
    }

    /// Apply per-axis extrapolation flags from an M1 project descriptor.
    ///
    /// Calibration exports carry sites and body values, while the project owns
    /// the boundary policy. Calling this method joins those two halves. Tables
    /// without an `Extrapolate` attribute keep the default clamped policy.
    pub fn apply_project_table_properties(
        &mut self,
        xml: &str,
        project: &Project,
    ) -> Result<(), EvalError> {
        let doc =
            roxmltree::Document::parse(xml).map_err(|error| EvalError::MissingCalibration {
                path: format!("project XML parse error while reading table properties: {error}"),
            })?;

        for component in doc
            .descendants()
            .filter(|node| node.has_tag_name("Component"))
        {
            let Some(project_name) = component.attribute("Name") else {
                continue;
            };
            if !project
                .symbols()
                .get(project_name)
                .is_some_and(|symbol| symbol.kind == SymbolKind::Table)
            {
                continue;
            }
            let config_name = project_name.strip_prefix("Root.").unwrap_or(project_name);
            let key = if self.tables.contains_key(project_name) {
                Some(project_name)
            } else if self.tables.contains_key(config_name) {
                Some(config_name)
            } else {
                None
            };
            let Some(key) = key else {
                continue;
            };
            // The pinned project model reads a direct `<Component><Axis>`
            // child, while real M1 project exports also place `<Axis>` inside
            // `<Props>`. Prefer the direct form if both are present, then accept
            // the evidenced export form.
            let axis_properties = component
                .children()
                .find(|node| node.has_tag_name("Axis"))
                .or_else(|| {
                    component
                        .children()
                        .find(|node| node.has_tag_name("Props"))
                        .and_then(|props| props.children().find(|node| node.has_tag_name("Axis")))
                });
            let table = self.tables.get_mut(key).expect("table key was checked");
            for (index, tag) in ["X", "Y", "Z"].into_iter().enumerate() {
                let Some(axis) = table.axes.get_mut(index) else {
                    break;
                };
                let properties = axis_properties.and_then(|properties| {
                    properties.children().find(|node| node.has_tag_name(tag))
                });
                if let Some(raw) = properties.and_then(|node| node.attribute("Extrapolate")) {
                    axis.extrapolation = parse_extrapolation(raw).ok_or_else(|| {
                        EvalError::MissingCalibration {
                            path: format!(
                                "table {project_name:?} axis {tag} has unknown Extrapolate value {raw:?}"
                            ),
                        }
                    })?;
                }
                if let CalAxisValues::Enum { values, enum_id } = &mut axis.values {
                    if axis.extrapolation != AxisExtrapolation::Clamp {
                        return Err(EvalError::MissingCalibration {
                            path: format!(
                                "table {project_name:?} axis {tag} is enumerated and cannot extrapolate"
                            ),
                        });
                    }
                    let source = properties
                        .and_then(|node| node.attribute("Source"))
                        .ok_or_else(|| EvalError::MissingCalibration {
                            path: format!(
                                "table {project_name:?} enum axis {tag} has no project Source"
                            ),
                        })?;
                    let resolved_id = resolve_enum_axis_source(project, project_name, tag, source)?;
                    let enum_type = project.symbols().enum_type(resolved_id);
                    if !enum_type.open {
                        for value in values {
                            if !enum_type
                                .members
                                .iter()
                                .any(|(_, declared)| declared == value)
                            {
                                return Err(EvalError::MissingCalibration {
                                    path: format!(
                                        "table {project_name:?} enum axis {tag} calibrates value {value}, which is not declared by enum {:?}",
                                        enum_type.name
                                    ),
                                });
                            }
                        }
                    }
                    *enum_id = Some(resolved_id);
                }
            }
        }
        Ok(())
    }
}

fn resolve_enum_axis_source(
    project: &Project,
    table: &str,
    axis: &str,
    source: &str,
) -> Result<EnumId, EvalError> {
    let scope = Scope {
        locals: HashMap::new(),
        group: Some(table.to_string()),
        project: Some(project),
        fn_symbol: None,
    };
    let symbol = match resolve(source, &scope) {
        Resolution::Symbol(symbol) => symbol,
        _ => {
            return Err(EvalError::MissingCalibration {
                path: format!(
                    "table {table:?} enum axis {axis} Source {source:?} does not resolve to a project value"
                ),
            });
        }
    };
    symbol
        .enum_assoc
        .ok_or_else(|| EvalError::MissingCalibration {
            path: format!(
                "table {table:?} enum axis {axis} Source {source:?} is not bound to an enum type"
            ),
        })
}

fn parse_extrapolation(raw: &str) -> Option<AxisExtrapolation> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" | "neither" | "false" | "clamp" => Some(AxisExtrapolation::Clamp),
        "below" => Some(AxisExtrapolation::Below),
        "above" => Some(AxisExtrapolation::Above),
        "both" => Some(AxisExtrapolation::Both),
        _ => None,
    }
}

/// Parse one calibration cell according to its declared M1 storage type.
/// Named enum members and Boolean cells are non-numeric and return `None`;
/// numeric enum representations return an integer. An absent type uses the
/// historical untyped-cell rule: numeric cells are parsed as binary32.
fn cell_value(
    cell: roxmltree::Node<'_, '_>,
    declared_type: Option<&str>,
    context: &str,
) -> Result<Option<M1Scalar>, EvalError> {
    let Some(text) = cell.text().map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let ty = declared_type.unwrap_or("untyped-f32");
    let width_error = || EvalError::MissingCalibration {
        path: format!("calibration cell in {context} does not fit M1 type {ty}: {text:?}"),
    };

    let scalar = match ty {
        "bool" => return Ok(None),
        "enum" => match text.parse::<f64>() {
            // Named members belong to the project enum model, not this numeric
            // calibration store. Keep the existing skip behavior for them.
            Err(_) => return Ok(None),
            Ok(value)
                if value.is_finite()
                    && value.fract() == 0.0
                    && value >= f64::from(i32::MIN)
                    && value <= f64::from(i32::MAX) =>
            {
                M1Scalar::Integer(value as i32)
            }
            Ok(_) => return Err(width_error()),
        },
        "f32" | "f64" | "untyped-f32" => {
            let narrowed = text.parse::<f32>().map_err(|_| width_error())?;
            if !narrowed.is_finite() {
                return Err(width_error());
            }
            M1Scalar::FloatingPoint(narrowed)
        }
        "s8" | "s16" | "s32" | "s64" => parse_calibration_i32(text)
            .map(M1Scalar::Integer)
            .ok_or_else(width_error)?,
        "u8" | "u16" | "u32" | "u64" => parse_unsigned_cell(text)
            .ok()
            .map(M1Scalar::UnsignedInteger)
            .ok_or_else(width_error)?,
        "FixedPoint7dps" | "fixed7dps" => {
            M1Scalar::FixedPoint7dps(FixedPoint7dps::parse_decimal(text).ok_or_else(width_error)?)
        }
        other => {
            return Err(EvalError::MissingCalibration {
                path: format!("unsupported calibration cell type {other:?} in {context}"),
            });
        }
    };
    Ok(Some(scalar))
}

/// Parse the unsigned literal forms emitted by M1 configuration exports.
/// Decimal and `0x` hexadecimal forms share the evaluator's 32-bit storage
/// limit. A trailing `u` is accepted for parity with script literals.
fn parse_unsigned_cell(text: &str) -> Result<u32, std::num::ParseIntError> {
    let lower = text.to_ascii_lowercase();
    let body = lower.strip_suffix('u').unwrap_or(&lower);
    let body = body.strip_prefix('+').unwrap_or(body);
    match body.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => body.parse::<u32>(),
    }
}

fn parse_calibration_i32(text: &str) -> Option<i32> {
    if let Ok(value) = text.parse::<i32>() {
        return Some(value);
    }
    let (negative, magnitude) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let hex = magnitude
        .strip_prefix("0x")
        .or_else(|| magnitude.strip_prefix("0X"))?;
    let magnitude = u32::from_str_radix(hex, 16).ok()?;
    if negative {
        i32::try_from(-(i64::from(magnitude))).ok()
    } else {
        i32::try_from(magnitude).ok()
    }
}

/// Parse the integer-valued decimal spelling used by enum axis cells. M1
/// exports these as decimal or scientific notation even though they are exact
/// enum values.
fn parse_enum_value(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let mut exponent_split = unsigned.split(['e', 'E']);
    let mantissa = exponent_split.next()?;
    let exponent = match exponent_split.next() {
        Some(value) if !value.is_empty() => value.parse::<i32>().ok()?,
        Some(_) => return None,
        None => 0,
    };
    if exponent_split.next().is_some() {
        return None;
    }
    let mut decimal_split = mantissa.split('.');
    let whole = decimal_split.next()?;
    let fractional = decimal_split.next().unwrap_or("");
    if decimal_split.next().is_some()
        || (whole.is_empty() && fractional.is_empty())
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    let mut digits = format!("{whole}{fractional}");
    if digits.bytes().all(|byte| byte == b'0') {
        return Some(0);
    }
    let shift = exponent.checked_sub(i32::try_from(fractional.len()).ok()?)?;
    if shift < 0 {
        let discarded = usize::try_from(shift.unsigned_abs()).ok()?;
        let kept = digits.len().checked_sub(discarded)?;
        if !digits[kept..].bytes().all(|byte| byte == b'0') {
            return None;
        }
        digits.truncate(kept);
    }
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Some(0);
    }
    let coefficient = significant.parse::<u128>().ok()?;
    let magnitude = if shift > 0 {
        coefficient.checked_mul(10_u128.checked_pow(shift as u32)?)?
    } else {
        coefficient
    };
    let magnitude = i128::try_from(magnitude).ok()?;
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).ok()
}

type Site = [usize; 3];

fn parse_site(cell: roxmltree::Node<'_, '_>, context: &str) -> Result<Option<Site>, EvalError> {
    let Some(raw) = cell.attribute("Site") else {
        return Ok(None);
    };
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(EvalError::MissingCalibration {
            path: format!("{context} has invalid Site coordinate {raw:?}"),
        });
    }
    let mut site = [0usize; 3];
    for (index, part) in parts.into_iter().enumerate() {
        site[index] = part
            .parse::<usize>()
            .map_err(|_| EvalError::MissingCalibration {
                path: format!("{context} has invalid Site coordinate {raw:?}"),
            })?;
    }
    Ok(Some(site))
}

fn order_axis_cells<T>(
    entries: Vec<(Option<Site>, T)>,
    axis_index: usize,
    context: &str,
) -> Result<Vec<T>, EvalError> {
    let with_site = entries.iter().filter(|(site, _)| site.is_some()).count();
    if with_site == 0 {
        return Ok(entries.into_iter().map(|(_, value)| value).collect());
    }
    if with_site != entries.len() {
        return Err(EvalError::MissingCalibration {
            path: format!("{context} mixes cells with and without Site coordinates"),
        });
    }

    let len = entries.len();
    let mut ordered: Vec<Option<T>> = (0..len).map(|_| None).collect();
    for (site, value) in entries {
        let site = site.expect("all entries were checked for Site coordinates");
        if site
            .iter()
            .enumerate()
            .any(|(index, coordinate)| index != axis_index && *coordinate != 0)
        {
            return Err(EvalError::MissingCalibration {
                path: format!(
                    "{context} cell Site={},{},{} changes a different axis",
                    site[0], site[1], site[2]
                ),
            });
        }
        let coordinate = site[axis_index];
        if coordinate >= len {
            return Err(EvalError::MissingCalibration {
                path: format!("{context} Site index {coordinate} is outside its {len}-site axis"),
            });
        }
        if ordered[coordinate].replace(value).is_some() {
            return Err(EvalError::MissingCalibration {
                path: format!("{context} declares Site index {coordinate} more than once"),
            });
        }
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            value.ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} is missing Site index {index}"),
            })
        })
        .collect()
}

fn parse_axis(
    axis: roxmltree::Node<'_, '_>,
    axis_index: usize,
    context: &str,
) -> Result<CalAxis, EvalError> {
    let Some(cells) = axis.children().find(|node| node.has_tag_name("Cells")) else {
        return Ok(CalAxis::numeric(Vec::new()));
    };
    let default_type = cells.attribute("Type");
    let nodes: Vec<_> = cells
        .children()
        .filter(|node| node.has_tag_name("Cell"))
        .collect();
    let enum_cells = nodes
        .iter()
        .filter(|cell| cell.attribute("Type").or(default_type) == Some("enum"))
        .count();
    if enum_cells != 0 && enum_cells != nodes.len() {
        return Err(EvalError::MissingCalibration {
            path: format!("{context} mixes enum and numeric sites"),
        });
    }

    if enum_cells == nodes.len() && !nodes.is_empty() {
        let mut entries = Vec::with_capacity(nodes.len());
        for cell in nodes {
            let raw = cell
                .text()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| EvalError::MissingCalibration {
                    path: format!("{context} has an empty enum site"),
                })?;
            let value = parse_enum_value(raw).ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} has invalid enum value {raw:?}"),
            })?;
            entries.push((parse_site(cell, context)?, value));
        }
        return order_axis_cells(entries, axis_index, context).map(CalAxis::enumerated);
    }

    let mut entries = Vec::with_capacity(nodes.len());
    for cell in nodes {
        let value = cell_value(cell, cell.attribute("Type").or(default_type), context)?
            .ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} contains a non-numeric site"),
            })?;
        entries.push((parse_site(cell, context)?, value));
    }
    order_axis_cells(entries, axis_index, context).map(CalAxis::numeric)
}

fn body_offset(site: Site, axes: &[CalAxis], context: &str) -> Result<usize, EvalError> {
    if site[axes.len()..].iter().any(|coordinate| *coordinate != 0) {
        return Err(EvalError::MissingCalibration {
            path: format!(
                "{context} Site={},{},{} addresses a disabled axis",
                site[0], site[1], site[2]
            ),
        });
    }
    let mut offset = 0usize;
    let mut stride = 1usize;
    for (index, axis) in axes.iter().enumerate() {
        if site[index] >= axis.len() {
            return Err(EvalError::MissingCalibration {
                path: format!(
                    "{context} Site={},{},{} is outside the table shape",
                    site[0], site[1], site[2]
                ),
            });
        }
        offset = offset
            .checked_add(site[index].checked_mul(stride).ok_or_else(|| {
                EvalError::MissingCalibration {
                    path: format!("{context} shape overflows a host index"),
                }
            })?)
            .ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} shape overflows a host index"),
            })?;
        stride = stride
            .checked_mul(axis.len())
            .ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} shape overflows a host index"),
            })?;
    }
    Ok(offset)
}

fn parse_body(
    body: Option<roxmltree::Node<'_, '_>>,
    axes: &[CalAxis],
    context: &str,
) -> Result<Vec<M1Scalar>, EvalError> {
    let Some(cells) = body.and_then(|body| body.children().find(|node| node.has_tag_name("Cells")))
    else {
        return Ok(Vec::new());
    };
    let default_type = cells.attribute("Type");
    let mut entries = Vec::new();
    for cell in cells.children().filter(|node| node.has_tag_name("Cell")) {
        let value = cell_value(cell, cell.attribute("Type").or(default_type), context)?
            .ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} contains a non-numeric cell"),
            })?;
        entries.push((parse_site(cell, context)?, value));
    }
    let with_site = entries.iter().filter(|(site, _)| site.is_some()).count();
    if with_site == 0 {
        // Compatibility path for hand-written fixtures. M1 exports include
        // explicit Site coordinates and use this same X-fastest order.
        return Ok(entries.into_iter().map(|(_, value)| value).collect());
    }
    if with_site != entries.len() {
        return Err(EvalError::MissingCalibration {
            path: format!("{context} mixes cells with and without Site coordinates"),
        });
    }

    let expected = axes.iter().try_fold(1usize, |product, axis| {
        product
            .checked_mul(axis.len())
            .ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} shape overflows a host index"),
            })
    })?;
    if entries.len() != expected {
        return Err(EvalError::MissingCalibration {
            path: format!(
                "{context} has {} addressed cells, expected {expected} for the axis shape",
                entries.len()
            ),
        });
    }
    let mut ordered: Vec<Option<M1Scalar>> = (0..expected).map(|_| None).collect();
    for (site, value) in entries {
        let offset = body_offset(
            site.expect("all body cells were checked for Site coordinates"),
            axes,
            context,
        )?;
        if ordered[offset].replace(value).is_some() {
            return Err(EvalError::MissingCalibration {
                path: format!("{context} declares body offset {offset} more than once"),
            });
        }
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(offset, value)| {
            value.ok_or_else(|| EvalError::MissingCalibration {
                path: format!("{context} is missing body offset {offset}"),
            })
        })
        .collect()
}

/// Parse a `<Table>` element into its concrete X/Y/Z axes and body.
fn parse_table(tbl: roxmltree::Node<'_, '_>) -> Result<CalTable, EvalError> {
    let name = tbl.attribute("Name").unwrap_or("<unnamed>");
    let mut axes = Vec::new();
    let mut found_gap = false;
    for (axis_index, tag) in ["X", "Y", "Z"].into_iter().enumerate() {
        match tbl.children().find(|node| node.has_tag_name(tag)) {
            Some(_) if found_gap => {
                return Err(EvalError::MissingCalibration {
                    path: format!("table {name:?} declares axis {tag} after a disabled axis"),
                });
            }
            Some(axis) => axes.push(parse_axis(
                axis,
                axis_index,
                &format!("table {name:?} axis {tag}"),
            )?),
            None => found_gap = true,
        }
    }
    let body = parse_body(
        tbl.children().find(|node| node.has_tag_name("Body")),
        &axes,
        &format!("table {name:?} body"),
    )?;
    let table = CalTable { axes, body };
    crate::table::validate_shape_named(&table, &format!("table {name:?}"))?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(value: f32) -> M1Scalar {
        M1Scalar::FloatingPoint(value)
    }

    fn numeric_values(axis: &CalAxis) -> &[M1Scalar] {
        match &axis.values {
            CalAxisValues::Numeric(values) => values,
            CalAxisValues::Enum { .. } => panic!("expected numeric axis"),
        }
    }

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
        assert_eq!(c.param("Root.A.Gain"), Some(f(2.5)));
        let t = c.table("Root.A.Map").unwrap();
        assert_eq!(t.axes.len(), 2);
        assert_eq!(numeric_values(&t.axes[0]), [f(0.0), f(100.0)]);
        assert_eq!(numeric_values(&t.axes[1]), [f(0.0), f(1.0)]);
        assert_eq!(t.body, vec![f(10.0), f(20.0), f(30.0), f(40.0)]);
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
  <Parameter Name="Outputs.Logging.Enabled">
   <Cell Type="bool"><![CDATA[true]]></Cell>
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
        // CDATA + scientific notation narrows to the declared binary32 value.
        assert_eq!(c.param("Inputs.APPS.APPS1.Offset"), Some(f(3.670_132_f32)));
        assert!(
            (c.param("Inputs.APPS.CompareThreshold").unwrap().as_f64() - 0.20000000298023224).abs()
                < 1e-12
        );
        // The enum cell is not a numeric calibration value: skipped, not guessed.
        assert_eq!(c.param("Outputs.Logging.LVLogging"), None);
        // Boolean storage is also outside this numeric calibration model and
        // must not make an otherwise valid configuration unloadable.
        assert_eq!(c.param("Outputs.Logging.Enabled"), None);
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
        assert_eq!(numeric_values(&t.axes[0]), [f(0.0), f(1.0), f(2.0)]);
        assert_eq!(t.body, vec![f(5.0), f(15.0), f(25.0)]);
    }

    #[test]
    fn numeric_enum_table_axis_uses_integer_representation() {
        let xml = r#"<Configuration>
          <Table Name="Root.Enum Curve">
            <X><Cells Type="enum"><Cell>0.0e+00</Cell><Cell>2.0e+00</Cell></Cells></X>
            <Body><Cells Type="f32"><Cell>5</Cell><Cell>25</Cell></Cells></Body>
          </Table>
        </Configuration>"#;
        let calibration = Calibration::from_m1cfg_str(xml).unwrap();
        let table = calibration.table("Root.Enum Curve").unwrap();
        assert_eq!(
            table.axes[0].values,
            CalAxisValues::Enum {
                values: vec![0, 2],
                enum_id: None,
            }
        );

        let fractional = r#"<Configuration>
          <Table Name="Root.Bad Enum Curve">
            <X><Cells Type="enum"><Cell>0.5</Cell></Cells></X>
          </Table>
        </Configuration>"#;
        let error = Calibration::from_m1cfg_str(fractional).unwrap_err();
        assert!(format!("{error}").contains("enum"), "{error}");
    }

    #[test]
    fn cell_types_preserve_signedness_and_reject_width_overflow() {
        let xml = r#"<Configuration>
          <Parameter Name="Signed"><Cell Type="s32">-2147483648</Cell></Parameter>
          <Parameter Name="Unsigned"><Cell Type="u32">4294967295</Cell></Parameter>
          <Parameter Name="Hex"><Cell Type="u32">0x0400</Cell></Parameter>
          <Parameter Name="Upper Hex"><Cell Type="u32">0XFFu</Cell></Parameter>
          <Parameter Name="Fixed"><Cell Type="FixedPoint7dps">1.2345678</Cell></Parameter>
        </Configuration>"#;
        let calibration = Calibration::from_m1cfg_str(xml).unwrap();
        assert_eq!(
            calibration.param("Signed"),
            Some(M1Scalar::Integer(i32::MIN))
        );
        assert_eq!(
            calibration.param("Unsigned"),
            Some(M1Scalar::UnsignedInteger(u32::MAX))
        );
        assert_eq!(
            calibration.param("Hex"),
            Some(M1Scalar::UnsignedInteger(0x400))
        );
        assert_eq!(
            calibration.param("Upper Hex"),
            Some(M1Scalar::UnsignedInteger(0xff))
        );
        assert_eq!(
            calibration.param("Fixed"),
            Some(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                12_345_678
            )))
        );

        let overflow = r#"<Configuration>
          <Parameter Name="Bad"><Cell Type="u32">4294967296</Cell></Parameter>
        </Configuration>"#;
        let error = Calibration::from_m1cfg_str(overflow).unwrap_err();
        assert!(format!("{error}").contains("u32"), "{error}");

        let float_widths = r#"<Configuration>
          <Parameter Name="Tiny"><Cell Type="f32">1e-50</Cell></Parameter>
        </Configuration>"#;
        let calibration = Calibration::from_m1cfg_str(float_widths).unwrap();
        assert_eq!(calibration.param("Tiny"), Some(f(0.0)));

        let float_overflow = r#"<Configuration>
          <Parameter Name="Huge"><Cell Type="f32">1e39</Cell></Parameter>
        </Configuration>"#;
        let error = Calibration::from_m1cfg_str(float_overflow).unwrap_err();
        assert!(format!("{error}").contains("f32"), "{error}");

        let host_overflow = r#"<Configuration>
          <Parameter Name="Huge"><Cell Type="f32">1e9999</Cell></Parameter>
        </Configuration>"#;
        let error = Calibration::from_m1cfg_str(host_overflow).unwrap_err();
        assert!(format!("{error}").contains("f32"), "{error}");
    }

    #[test]
    fn site_coordinates_define_axis_and_x_fastest_body_order() {
        let xml = r#"<Configuration>
          <Table Name="Map">
            <X><Cells Type="f32">
              <Cell Site="1,0,0">10</Cell><Cell Site="0,0,0">0</Cell>
            </Cells></X>
            <Y><Cells Type="f32">
              <Cell Site="0,1,0">1</Cell><Cell Site="0,0,0">0</Cell>
            </Cells></Y>
            <Body><Cells Type="f32">
              <Cell Site="1,1,0">40</Cell><Cell Site="0,1,0">20</Cell>
              <Cell Site="1,0,0">30</Cell><Cell Site="0,0,0">10</Cell>
            </Cells></Body>
          </Table>
        </Configuration>"#;
        let calibration = Calibration::from_m1cfg_str(xml).unwrap();
        let table = calibration.table("Map").unwrap();
        assert_eq!(numeric_values(&table.axes[0]), [f(0.0), f(10.0)]);
        assert_eq!(numeric_values(&table.axes[1]), [f(0.0), f(1.0)]);
        assert_eq!(table.body, vec![f(10.0), f(30.0), f(20.0), f(40.0)]);
    }

    #[test]
    fn enum_axis_values_keep_site_order() {
        assert_eq!(parse_enum_value("0e-999"), Some(0));
        assert_eq!(parse_enum_value("-2.000e+0"), Some(-2));
        assert_eq!(parse_enum_value("1.5"), None);

        let xml = r#"<Configuration>
          <Table Name="Modes">
            <X><Cells Type="enum">
              <Cell Site="1,0,0">2.000000000e+00</Cell><Cell Site="0,0,0">0</Cell>
            </Cells></X>
            <Body><Cells Type="f32">
              <Cell Site="1,0,0">20</Cell><Cell Site="0,0,0">10</Cell>
            </Cells></Body>
          </Table>
        </Configuration>"#;
        let calibration = Calibration::from_m1cfg_str(xml).unwrap();
        let table = calibration.table("Modes").unwrap();
        assert_eq!(
            table.axes[0].values,
            CalAxisValues::Enum {
                values: vec![0, 2],
                enum_id: None,
            }
        );
        assert_eq!(table.body, vec![f(10.0), f(20.0)]);
    }

    #[test]
    fn canonical_project_axis_properties_cover_every_table_class() {
        for classname in ["BuiltIn.TableVariant", "BuiltIn.CalibrationTable"] {
            let mut calibration = Calibration::from_m1cfg_str(XML).unwrap();
            let project_xml = format!(
                r#"<MoTeCM1BuildSession><Project><ComponentStream><List>
                  <Component Classname="{classname}" Name="Root.A.Map">
                    <Props Type="f32" NumAxes="2"/>
                    <Axis><X Extrapolate="Below"/><Y Extrapolate="Above"/></Axis>
                  </Component>
                </List></ComponentStream></Project></MoTeCM1BuildSession>"#
            );
            let temp = tempfile::tempdir().unwrap();
            let project_path = temp.path().join("Project.m1prj");
            std::fs::write(&project_path, &project_xml).unwrap();
            let project = Project::load(&project_path).unwrap();

            calibration
                .apply_project_table_properties(&project_xml, &project)
                .unwrap();
            let table = calibration.table("Root.A.Map").unwrap();
            assert_eq!(table.axes[0].extrapolation, AxisExtrapolation::Below);
            assert_eq!(table.axes[1].extrapolation, AxisExtrapolation::Above);
        }
    }

    #[test]
    fn closed_enum_axes_reject_undeclared_sites_but_open_enums_accept_them() {
        let calibration_xml = r#"<Configuration><Table Name="A.Map">
          <X><Cells Type="enum"><Cell>0</Cell><Cell>99</Cell></Cells></X>
          <Body><Cells Type="f32"><Cell>1</Cell><Cell>2</Cell></Cells></Body>
        </Table></Configuration>"#;
        let project_xml = |type_declaration: &str, source_type: &str| {
            format!(
                r#"<MoTeCM1BuildSession><Project>
                  <DataTypes>{type_declaration}</DataTypes>
                  <ComponentStream><List>
                    <Component Classname="BuiltIn.GroupCompound" Name="Root.A"/>
                    <Component Classname="BuiltIn.Channel" Name="Root.A.Mode"><Props Type="{source_type}"/></Component>
                    <Component Classname="BuiltIn.Table" Name="Root.A.Map">
                      <Props Type="f32" NumAxes="1"/>
                      <Axis><X Source="Parent.Mode"/></Axis>
                    </Component>
                  </List></ComponentStream>
                </Project></MoTeCM1BuildSession>"#
            )
        };
        let load_project = |xml: &str| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("Project.m1prj");
            std::fs::write(&path, xml).unwrap();
            (temp, Project::load(&path).unwrap())
        };

        let closed_xml = project_xml(
            r#"<Type Storage="enum" Name="Mode" Default="Idle">
              <Enum Name="Idle" ContainerOrder="0"/>
            </Type>"#,
            "::This.Mode",
        );
        let (_closed_temp, closed_project) = load_project(&closed_xml);
        let mut closed = Calibration::from_m1cfg_str(calibration_xml).unwrap();
        let error = closed
            .apply_project_table_properties(&closed_xml, &closed_project)
            .expect_err("closed enum excludes calibration value 99");
        assert!(format!("{error}").contains("value 99"), "{error}");

        let open_xml = project_xml("", "MoTeC Types.Undocumented Table Mode");
        let (_open_temp, open_project) = load_project(&open_xml);
        let mut open = Calibration::from_m1cfg_str(calibration_xml).unwrap();
        open.apply_project_table_properties(&open_xml, &open_project)
            .expect("open firmware enum membership is not exhaustively known");
        let CalAxisValues::Enum {
            enum_id: Some(enum_id),
            ..
        } = &open.table("A.Map").unwrap().axes[0].values
        else {
            panic!("enum axis is bound to its open project type");
        };
        assert!(open_project.symbols().enum_is_open(*enum_id));
    }

    #[test]
    fn sparse_addressed_body_rejects_shape_before_large_allocation() {
        let sites = (0..1_000)
            .map(|value| format!("<Cell>{value}</Cell>"))
            .collect::<String>();
        let xml = format!(
            r#"<Configuration><Table Name="Sparse">
              <X><Cells Type="f32">{sites}</Cells></X>
              <Y><Cells Type="f32">{sites}</Cells></Y>
              <Z><Cells Type="f32">{sites}</Cells></Z>
              <Body><Cells Type="f32"><Cell Site="0,0,0">1</Cell></Cells></Body>
            </Table></Configuration>"#
        );
        let error = Calibration::from_m1cfg_str(&xml).unwrap_err();
        assert!(
            format!("{error}").contains("1 addressed cells, expected 1000000000"),
            "{error}"
        );
    }

    #[test]
    fn invalid_sites_axes_and_shapes_fail_during_calibration_load() {
        let cases = [
            (
                r#"<Configuration><Table Name="Gap"><X><Cells Type="f32"><Cell>0</Cell></Cells></X><Z><Cells Type="f32"><Cell>0</Cell></Cells></Z><Body><Cells Type="f32"><Cell>1</Cell></Cells></Body></Table></Configuration>"#,
                "after a disabled axis",
            ),
            (
                r#"<Configuration><Table Name="Mixed"><X><Cells Type="f32"><Cell Site="0,0,0">0</Cell><Cell>1</Cell></Cells></X><Body><Cells Type="f32"><Cell>1</Cell><Cell>2</Cell></Cells></Body></Table></Configuration>"#,
                "mixes cells with and without Site",
            ),
            (
                r#"<Configuration><Table Name="DuplicateAxis"><X><Cells Type="f32"><Cell Site="0,0,0">0</Cell><Cell Site="0,0,0">1</Cell></Cells></X><Body><Cells Type="f32"><Cell>1</Cell><Cell>2</Cell></Cells></Body></Table></Configuration>"#,
                "Site index 0 more than once",
            ),
            (
                r#"<Configuration><Table Name="OutsideBody"><X><Cells Type="f32"><Cell>0</Cell><Cell>1</Cell></Cells></X><Body><Cells Type="f32"><Cell Site="0,0,0">1</Cell><Cell Site="2,0,0">2</Cell></Cells></Body></Table></Configuration>"#,
                "outside the table shape",
            ),
            (
                r#"<Configuration><Table Name="DuplicateBody"><X><Cells Type="f32"><Cell>0</Cell><Cell>1</Cell></Cells></X><Body><Cells Type="f32"><Cell Site="0,0,0">1</Cell><Cell Site="0,0,0">2</Cell></Cells></Body></Table></Configuration>"#,
                "body offset 0 more than once",
            ),
            (
                r#"<Configuration><Table Name="DisabledAxis"><X><Cells Type="f32"><Cell>0</Cell></Cells></X><Body><Cells Type="f32"><Cell Site="0,1,0">1</Cell></Cells></Body></Table></Configuration>"#,
                "addresses a disabled axis",
            ),
            (
                r#"<Configuration><Table Name="Shape"><X><Cells Type="f32"><Cell>0</Cell><Cell>1</Cell></Cells></X><Body><Cells Type="f32"><Cell>1</Cell></Cells></Body></Table></Configuration>"#,
                "expected 2",
            ),
            (
                r#"<Configuration><Table Name="Zero"><Body><Cells Type="f32"><Cell>1</Cell></Cells></Body></Table></Configuration>"#,
                "require one, two, or three",
            ),
        ];
        for (xml, needle) in cases {
            let error = Calibration::from_m1cfg_str(xml).expect_err("invalid table must fail");
            assert!(format!("{error}").contains(needle), "{error}");
        }
    }
}
