// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic whole-project schedule planning.
//!
//! The plan is an evaluator contract, not a claim about M1 compiler parity.
//! Trigger roles and rates come from [`crate::TriggerMap`]. Channel dependency
//! edges come from the same CST I/O analysis used by the runners. The planner
//! keeps edges across rates, rejects ambiguous periodic writers, and rejects
//! cycles instead of choosing an order that has not been captured from M1.

use crate::error::EvalError;
use crate::loader::Loaded;
use crate::summary::{CheckedIoSets, checked_io_sets};
use crate::triggers::TriggerStatus;
use m1_typecheck::symbols::{Symbol, SymbolKind};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// Evidence status of the schedule ordering contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScheduleMaturity {
    /// Synthetic fixtures establish determinism, but no genuine M1 schedule
    /// capture establishes compiler parity.
    Assumed,
}

/// How the planner orders ready functions that have no dependency between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadyTiePolicy {
    /// Prefer the higher periodic rate, then the canonical function name.
    /// This policy is isolated so captured M1 evidence can replace it without
    /// changing dependency construction.
    RateDescendingThenFunction,
}

/// One script-backed function and its role in a whole-project schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulePlanEntry {
    /// Canonical function symbol.
    pub function: String,
    /// Resolved trigger role from the loaded project's [`crate::TriggerMap`].
    pub trigger: TriggerStatus,
    /// Zero-based position in the global periodic order. Nonperiodic functions
    /// have no position.
    pub order: Option<usize>,
}

impl SchedulePlanEntry {
    /// The periodic rate, or `None` for startup, helper, unscheduled, and
    /// unresolved functions.
    pub fn periodic_rate(&self) -> Option<f64> {
        match self.trigger {
            TriggerStatus::Periodic(rate_hz) => Some(rate_hz),
            _ => None,
        }
    }
}

/// One global writer-before-reader dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDependency {
    /// Periodic function that owns the written channels.
    pub writer: String,
    /// Periodic function that reads those channels.
    pub reader: String,
    /// Canonical channel paths shared by this writer and reader.
    pub channels: Vec<String>,
}

/// The owned, deterministic whole-project schedule description.
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulePlan {
    /// Current evidence status. This remains `Assumed` until genuine M1
    /// schedule captures establish the ordering contract.
    pub maturity: ScheduleMaturity,
    /// Policy used only when two ready nodes have no dependency ordering.
    pub ready_tie_policy: ReadyTiePolicy,
    /// Every script-backed function. Periodic entries appear in execution order,
    /// followed by nonperiodic entries in canonical-name order.
    pub entries: Vec<SchedulePlanEntry>,
    /// Global periodic dependencies in writer, reader, channel order.
    pub dependencies: Vec<ScheduleDependency>,
}

impl SchedulePlan {
    /// Periodic entries in their global execution order.
    pub fn periodic_entries(&self) -> impl Iterator<Item = &SchedulePlanEntry> {
        self.entries.iter().filter(|entry| entry.order.is_some())
    }

    /// Dependencies whose reader is `function`.
    pub fn dependencies_for_reader(
        &self,
        function: &str,
    ) -> impl Iterator<Item = &ScheduleDependency> {
        self.dependencies
            .iter()
            .filter(move |dependency| dependency.reader == function)
    }
}

/// Build the schedule plan consumed by runtime and loaded-project coverage.
pub fn build_schedule_plan(loaded: &Loaded) -> Result<SchedulePlan, EvalError> {
    validate_script_bindings(loaded)?;

    let mut entries = Vec::new();
    let mut direct_io = BTreeMap::new();

    for script in &loaded.scripts {
        let Some(function) = loaded.project.function_symbol_for_script(&script.name) else {
            continue;
        };
        if direct_io.contains_key(&function) {
            return Err(schedule_error(format!(
                "function `{function}` is backed by more than one script"
            )));
        }
        let group = loaded.project.group_for_script(&script.name);
        let io = checked_io_sets(script, &loaded.project, group.as_deref());
        direct_io.insert(function.clone(), io);
        let trigger =
            loaded
                .triggers
                .get(&function)
                .cloned()
                .unwrap_or_else(|| TriggerStatus::Unresolved {
                    trigger: String::new(),
                    reason: "the loaded trigger map has no entry for this script-backed function"
                        .to_string(),
                });
        entries.push(SchedulePlanEntry {
            function,
            trigger,
            order: None,
        });
    }

    let periodic: BTreeMap<String, f64> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .periodic_rate()
                .map(|rate| (entry.function.clone(), rate))
        })
        .collect();
    for (function, rate_hz) in &periodic {
        if !rate_hz.is_finite() || *rate_hz <= 0.0 {
            return Err(schedule_error(format!(
                "function `{function}` has invalid periodic rate {rate_hz} Hz"
            )));
        }
    }

    let mut effective_io = BTreeMap::new();
    let mut call_io_cache = BTreeMap::new();
    for function in periodic.keys() {
        let mut stack = Vec::new();
        let io = effective_root_io(function, &direct_io, &mut call_io_cache, &mut stack)?;
        effective_io.insert(function.clone(), io);
    }

    let mut writers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (function, io) in &effective_io {
        for channel in &io.writes {
            writers
                .entry(channel.clone())
                .or_default()
                .push(function.clone());
        }
    }
    for (channel, owners) in &writers {
        if owners.len() > 1 {
            return Err(schedule_error(format!(
                "channel `{channel}` has multiple periodic writers: {}; refusing to guess an execution order",
                owners.join(", ")
            )));
        }
    }

    let mut edge_channels: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (reader, io) in &effective_io {
        for channel in &io.reads {
            let Some(owner) = writers.get(channel).and_then(|owners| owners.first()) else {
                continue;
            };
            if owner != reader {
                edge_channels
                    .entry((owner.clone(), reader.clone()))
                    .or_default()
                    .insert(channel.clone());
            }
        }
    }

    let dependencies: Vec<ScheduleDependency> = edge_channels
        .iter()
        .map(|((writer, reader), channels)| ScheduleDependency {
            writer: writer.clone(),
            reader: reader.clone(),
            channels: channels.iter().cloned().collect(),
        })
        .collect();
    let ordered = topological_periodic_order(&periodic, &dependencies)?;
    let order_by_function: BTreeMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(order, function)| (function.as_str(), order))
        .collect();
    for entry in &mut entries {
        entry.order = order_by_function.get(entry.function.as_str()).copied();
    }
    entries.sort_by(|left, right| match (left.order, right.order) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.function.cmp(&right.function),
    });

    Ok(SchedulePlan {
        maturity: ScheduleMaturity::Assumed,
        ready_tie_policy: ReadyTiePolicy::RateDescendingThenFunction,
        entries,
        dependencies,
    })
}

fn validate_script_bindings(loaded: &Loaded) -> Result<(), EvalError> {
    let mut parsed_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for script in &loaded.scripts {
        *parsed_counts.entry(script.name.as_str()).or_default() += 1;
    }

    let mut explicit_symbols_by_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for symbol in loaded.project.symbols().iter().filter(|symbol| {
        matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method)
            && symbol.filename.is_some()
    }) {
        explicit_symbols_by_file
            .entry(symbol.filename.as_deref().expect("filename checked above"))
            .or_default()
            .push(symbol.path.as_str());
    }

    for symbol in loaded
        .project
        .symbols()
        .iter()
        .filter(|symbol| symbol.scheduled)
        .filter(|symbol| symbol.filename.is_some())
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method))
    {
        let Some(file_name) = script_filename_for_symbol(symbol) else {
            return Err(schedule_error(format!(
                "scheduled function `{}` has no script filename",
                symbol.path
            )));
        };
        if let Some(symbols) = explicit_symbols_by_file.get(file_name.as_str())
            && symbols.len() > 1
        {
            return Err(schedule_error(format!(
                "script filename `{file_name}` is bound to multiple Function/Method symbols: {}; instance-aware script binding is not implemented",
                symbols.join(", ")
            )));
        }
        match parsed_counts.get(file_name.as_str()).copied().unwrap_or(0) {
            1 => {}
            0 => {
                return Err(schedule_error(format!(
                    "scheduled function `{}` is bound to missing script `{file_name}`",
                    symbol.path
                )));
            }
            count => {
                return Err(schedule_error(format!(
                    "scheduled function `{}` is bound to script `{file_name}` parsed {count} times",
                    symbol.path
                )));
            }
        }
        if loaded
            .project
            .function_symbol_for_script(file_name.as_str())
            .as_deref()
            != Some(symbol.path.as_str())
        {
            return Err(schedule_error(format!(
                "scheduled function `{}` is not the unique project binding for script `{file_name}`",
                symbol.path
            )));
        }
    }
    Ok(())
}

fn script_filename_for_symbol(symbol: &Symbol) -> Option<String> {
    symbol.filename.clone().or_else(|| {
        symbol
            .path
            .strip_prefix("Root.")
            .map(|stem| format!("{stem}.m1scr"))
    })
}

/// Fold every transitively called script-backed function's I/O into one
/// periodic root. Runtime dispatch inlines every such callee regardless of its
/// trigger role, so excluding a reachable callee would hide real channel access.
fn effective_root_io(
    function: &str,
    direct_io: &BTreeMap<String, CheckedIoSets>,
    call_io_cache: &mut BTreeMap<String, CheckedIoSets>,
    stack: &mut Vec<String>,
) -> Result<CheckedIoSets, EvalError> {
    if let Some(cached) = call_io_cache.get(function) {
        return Ok(cached.clone());
    }
    if let Some(start) = stack.iter().position(|item| item == function) {
        let mut cycle = stack[start..].to_vec();
        cycle.push(function.to_string());
        return Err(schedule_error(format!(
            "script-backed call cycle prevents schedule analysis: {}",
            cycle.join(" -> ")
        )));
    }
    let Some(direct) = direct_io.get(function) else {
        return Err(schedule_error(format!(
            "script-backed function `{function}` has no I/O summary"
        )));
    };
    if let Some(reason) = direct.analysis_errors().next() {
        return Err(schedule_error(format!(
            "reachable function `{function}` has incomplete static I/O: {reason}"
        )));
    }

    stack.push(function.to_string());
    let mut result = direct.clone();
    for callee in &direct.calls {
        if !direct_io.contains_key(callee) {
            return Err(schedule_error(format!(
                "function `{callee}` called by `{function}` has no script I/O summary"
            )));
        }
        let callee_io = effective_root_io(callee, direct_io, call_io_cache, stack)?;
        result.sets.reads.extend(callee_io.sets.reads);
        result.sets.writes.extend(callee_io.sets.writes);
        result.calls.extend(callee_io.calls);
    }
    stack.pop();
    call_io_cache.insert(function.to_string(), result.clone());
    Ok(result)
}

fn topological_periodic_order(
    rates: &BTreeMap<String, f64>,
    dependencies: &[ScheduleDependency],
) -> Result<Vec<String>, EvalError> {
    let mut indegree: BTreeMap<String, usize> =
        rates.keys().map(|function| (function.clone(), 0)).collect();
    let mut successors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for dependency in dependencies {
        if successors
            .entry(dependency.writer.clone())
            .or_default()
            .insert(dependency.reader.clone())
        {
            *indegree
                .get_mut(&dependency.reader)
                .expect("dependency reader is a periodic function") += 1;
        }
    }

    let mut ready: Vec<String> = indegree
        .iter()
        .filter_map(|(function, degree)| (*degree == 0).then_some(function.clone()))
        .collect();
    let mut ordered = Vec::with_capacity(rates.len());
    while !ready.is_empty() {
        ready.sort_by(|left, right| assumed_ready_order(left, right, rates));
        let function = ready.remove(0);
        ordered.push(function.clone());
        if let Some(next) = successors.get(&function) {
            for successor in next {
                let degree = indegree
                    .get_mut(successor)
                    .expect("dependency successor is periodic");
                *degree -= 1;
                if *degree == 0 {
                    ready.push(successor.clone());
                }
            }
        }
    }

    if ordered.len() != rates.len() {
        let cycle_path = dependency_cycle(rates, dependencies);
        let cycle_nodes: BTreeSet<&str> = cycle_path
            .as_ref()
            .map(|path| {
                path.iter()
                    .take(path.len().saturating_sub(1))
                    .map(String::as_str)
                    .collect()
            })
            .unwrap_or_default();
        let blocked: Vec<&str> = indegree
            .iter()
            .filter_map(|(function, degree)| {
                (*degree > 0 && !cycle_nodes.contains(function.as_str()))
                    .then_some(function.as_str())
            })
            .collect();
        let cycle = cycle_path
            .as_ref()
            .map(|path| format_dependency_cycle(path, dependencies))
            .unwrap_or_else(|| "unidentified cycle".to_string());
        let blocked_suffix = if blocked.is_empty() {
            String::new()
        } else {
            format!(
                "; downstream blocked periodic functions: {}",
                blocked.join(", ")
            )
        };
        return Err(schedule_error(format!(
            "periodic dependency cycle has no verified execution order: {cycle}{blocked_suffix}"
        )));
    }
    Ok(ordered)
}

fn format_dependency_cycle(path: &[String], dependencies: &[ScheduleDependency]) -> String {
    let Some(first) = path.first() else {
        return "unidentified cycle".to_string();
    };
    let mut out = first.clone();
    for pair in path.windows(2) {
        let channels = dependencies
            .iter()
            .find(|dependency| dependency.writer == pair[0] && dependency.reader == pair[1])
            .map(|dependency| dependency.channels.join(", "))
            .filter(|channels| !channels.is_empty())
            .unwrap_or_else(|| "?".to_string());
        out.push_str(&format!(" -[{channels}]-> {}", pair[1]));
    }
    out
}

fn dependency_cycle(
    rates: &BTreeMap<String, f64>,
    dependencies: &[ScheduleDependency],
) -> Option<Vec<String>> {
    let mut successors: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for dependency in dependencies {
        successors
            .entry(&dependency.writer)
            .or_default()
            .insert(&dependency.reader);
    }
    let mut state: BTreeMap<&str, u8> = rates.keys().map(|name| (name.as_str(), 0)).collect();
    let mut stack = Vec::new();
    for function in rates.keys() {
        if state[function.as_str()] == 0
            && let Some(cycle) = visit_for_cycle(function, &successors, &mut state, &mut stack)
        {
            return Some(cycle);
        }
    }
    None
}

fn visit_for_cycle<'a>(
    function: &'a str,
    successors: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    state: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    state.insert(function, 1);
    stack.push(function);
    for &successor in successors.get(function).into_iter().flatten() {
        match state.get(successor).copied().unwrap_or(0) {
            0 => {
                if let Some(cycle) = visit_for_cycle(successor, successors, state, stack) {
                    return Some(cycle);
                }
            }
            1 => {
                let start = stack
                    .iter()
                    .position(|item| *item == successor)
                    .expect("active successor is on the DFS stack");
                let mut cycle: Vec<String> = stack[start..]
                    .iter()
                    .map(|item| (*item).to_string())
                    .collect();
                cycle.push(successor.to_string());
                return Some(cycle);
            }
            _ => {}
        }
    }
    stack.pop();
    state.insert(function, 2);
    None
}

/// Assumed fallback for incomparable ready nodes. This is intentionally the
/// only place that encodes a ready-node tie policy. A genuine M1 schedule
/// capture can replace this comparator without touching graph construction.
fn assumed_ready_order(left: &str, right: &str, rates: &BTreeMap<String, f64>) -> Ordering {
    rates[right]
        .total_cmp(&rates[left])
        .then_with(|| left.cmp(right))
}

fn schedule_error(kind: String) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: format!("whole-project schedule: {kind}"),
        at: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::load;
    use std::path::Path;

    fn write_project(project: &Path, body: &str, scripts: &[(&str, &str)]) {
        std::fs::write(project.join("Project.m1prj"), body).expect("write project");
        let scripts_dir = project.join("Scripts");
        std::fs::create_dir(&scripts_dir).expect("create scripts dir");
        for (name, source) in scripts {
            std::fs::write(scripts_dir.join(name), source).expect("write script");
        }
    }

    fn project_xml(extra: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Schedule Tests" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.T"/>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Events"/>
   <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 100Hz"/>
   <Component Classname="BuiltIn.Channel" Name="Root.T.A"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.T.B"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.T.C"><Props Type="f32"/></Component>
{extra}
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#
        )
    }

    fn load_project(extra: &str, scripts: &[(&str, &str)]) -> Loaded {
        let temp = tempfile::tempdir().expect("temp project");
        write_project(temp.path(), &project_xml(extra), scripts);
        load(&temp.path().join("Project.m1prj"), None).expect("project loads")
    }

    fn expect_plan_error(extra: &str, source: &str, expected: &str) {
        let loaded = load_project(extra, &[("T.Writer.m1scr", source)]);
        let error = build_schedule_plan(&loaded).expect_err("plan must fail loud");
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }

    #[test]
    fn cycle_error_names_each_dependency_channel() {
        let temp = tempfile::tempdir().expect("temp project");
        write_project(
            temp.path(),
            &project_xml(
                r#"   <Component Classname="BuiltIn.FuncUser" Filename="T.One.m1scr" Name="Root.T.One"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Two.m1scr" Name="Root.T.Two"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Blocked.m1scr" Name="Root.T.Blocked"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>"#,
            ),
            &[
                ("T.One.m1scr", "A = B;\n"),
                ("T.Two.m1scr", "B = A;\n"),
                ("T.Blocked.m1scr", "C = A;\n"),
            ],
        );
        let loaded = load(&temp.path().join("Project.m1prj"), None).expect("project loads");
        let error = build_schedule_plan(&loaded).expect_err("cycle fails loud");
        let text = error.to_string();

        assert!(
            text.contains("Root.T.One -[Root.T.A]-> Root.T.Two")
                && text.contains("Root.T.Two -[Root.T.B]-> Root.T.One"),
            "cycle edge channels should be actionable: {text}"
        );
        assert!(
            text.contains("downstream blocked periodic functions: Root.T.Blocked"),
            "blocked non-cycle node should be separate: {text}"
        );
    }

    #[test]
    fn duplicate_explicit_script_binding_fails_loud() {
        let temp = tempfile::tempdir().expect("temp project");
        write_project(
            temp.path(),
            &project_xml(
                r#"   <Component Classname="BuiltIn.FuncUser" Filename="T.Shared.m1scr" Name="Root.T.One"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Shared.m1scr" Name="Root.T.Two"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>"#,
            ),
            &[("T.Shared.m1scr", "A = B;\n")],
        );
        let loaded = load(&temp.path().join("Project.m1prj"), None).expect("project loads");
        let error = build_schedule_plan(&loaded).expect_err("ambiguous script binding fails");
        let text = error.to_string();

        assert!(text.contains("T.Shared.m1scr"), "{text}");
        assert!(
            text.contains("Root.T.One") && text.contains("Root.T.Two"),
            "{text}"
        );
        assert!(
            text.contains("instance-aware script binding is not implemented"),
            "{text}"
        );
    }

    #[test]
    fn scheduled_function_with_missing_script_fails_loud() {
        let temp = tempfile::tempdir().expect("temp project");
        write_project(
            temp.path(),
            &project_xml(
                r#"   <Component Classname="BuiltIn.FuncUser" Filename="T.Missing.m1scr" Name="Root.T.Missing"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>"#,
            ),
            &[],
        );
        let loaded = load(&temp.path().join("Project.m1prj"), None).expect("project loads");
        let error = build_schedule_plan(&loaded).expect_err("missing scheduled script fails");
        let text = error.to_string();

        assert!(text.contains("Root.T.Missing"), "{text}");
        assert!(text.contains("T.Missing.m1scr"), "{text}");
        assert!(text.contains("missing script"), "{text}");
    }

    #[test]
    fn nested_expand_templates_create_every_dependency_channel() {
        let loaded = load_project(
            r#"
   <Component Classname="BuiltIn.Channel" Name="Root.T.Shared 1 1"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.T.Shared 1 2"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.T.Shared 2 1"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.T.Shared 2 2"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Writer.m1scr" Name="Root.T.Writer"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Reader.m1scr" Name="Root.T.Reader"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>"#,
            &[
                (
                    "T.Writer.m1scr",
                    "expand (I = 1 to 2)\n{\n\texpand (J = 1 to 2)\n\t{\n\t\tShared $(I) $(J) = 1.0;\n\t}\n}\n",
                ),
                (
                    "T.Reader.m1scr",
                    "expand (I = 1 to 2)\n{\n\texpand (J = 1 to 2)\n\t{\n\t\tB = Shared $(I) $(J);\n\t}\n}\n",
                ),
            ],
        );

        let plan = build_schedule_plan(&loaded).expect("nested literal expands plan");
        assert_eq!(
            plan.dependencies,
            vec![ScheduleDependency {
                writer: "Root.T.Writer".to_string(),
                reader: "Root.T.Reader".to_string(),
                channels: vec![
                    "Root.T.Shared 1 1".to_string(),
                    "Root.T.Shared 1 2".to_string(),
                    "Root.T.Shared 2 1".to_string(),
                    "Root.T.Shared 2 2".to_string(),
                ],
            }]
        );
    }

    #[test]
    fn incomplete_expand_forms_fail_loud_for_a_periodic_root() {
        let extra = r#"
   <Component Classname="BuiltIn.FuncUser" Filename="T.Writer.m1scr" Name="Root.T.Writer"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>"#;

        expect_plan_error(
            extra,
            "expand (I = 0 to Count)\n{\n\tA $(I) = 1.0;\n}\n",
            "end bound `Count` is not a literal integer",
        );
        expect_plan_error(
            extra,
            "expand (I = 2 to 1)\n{\n\tA $(I) = 1.0;\n}\n",
            "descending range 2 to 1",
        );
        expect_plan_error(extra, "A $(MISSING) = 1.0;\n", "unresolved template");
        expect_plan_error(
            extra,
            "expand (I = 0 to 64)\n{\n\texpand (J = 0 to 64)\n\t{\n\t\tA $(I) $(J) = 1.0;\n\t}\n}\n",
            "exceeds 4096 variants",
        );
        expect_plan_error(
            extra,
            "expand (I = 0 to 1)\n{\n\texpand (I = 0 to 1)\n\t{\n\t\tA $(I) = 1.0;\n\t}\n}\n",
            "shadows loop variable `I`",
        );
    }

    #[test]
    fn periodic_root_inherits_transitive_script_backed_callee_io() {
        let loaded = load_project(
            r#"
   <Component Classname="BuiltIn.FuncUser" Filename="T.Writer.m1scr" Name="Root.T.Writer"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Reader.m1scr" Name="Root.T.Reader"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUserParam" Filename="T.First.m1scr" Name="Root.T.First"/>
   <Component Classname="BuiltIn.FuncUserParam" Filename="T.Second.m1scr" Name="Root.T.Second"/>"#,
            &[
                ("T.Writer.m1scr", "First();\n"),
                ("T.Reader.m1scr", "B = A;\n"),
                ("T.First.m1scr", "Second();\n"),
                ("T.Second.m1scr", "A = 1.0;\n"),
            ],
        );

        let plan = build_schedule_plan(&loaded).expect("callee closure plans");
        assert_eq!(
            plan.dependencies,
            vec![ScheduleDependency {
                writer: "Root.T.Writer".to_string(),
                reader: "Root.T.Reader".to_string(),
                channels: vec!["Root.T.A".to_string()],
            }]
        );
    }

    #[test]
    fn reachable_helper_analysis_error_blocks_while_orphan_does_not() {
        let common = r#"
   <Component Classname="BuiltIn.FuncUser" Filename="T.Writer.m1scr" Name="Root.T.Writer"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="T.Reader.m1scr" Name="Root.T.Reader"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
   <Component Classname="BuiltIn.FuncUserParam" Filename="T.Helper.m1scr" Name="Root.T.Helper"/>
   <Component Classname="BuiltIn.FuncUserParam" Filename="T.Orphan.m1scr" Name="Root.T.Orphan"/>"#;
        let scripts = [
            ("T.Writer.m1scr", "Helper();\n"),
            ("T.Reader.m1scr", "B = A;\n"),
            (
                "T.Helper.m1scr",
                "expand (I = 2 to 1)\n{\n\tA $(I) = 1.0;\n}\n",
            ),
            (
                "T.Orphan.m1scr",
                "expand (I = 2 to 1)\n{\n\tA $(I) = 1.0;\n}\n",
            ),
        ];
        let loaded = load_project(common, &scripts);
        let error = build_schedule_plan(&loaded).expect_err("reachable helper must fail loud");
        assert!(
            error
                .to_string()
                .contains("reachable function `Root.T.Helper`"),
            "{error}"
        );

        let loaded = load_project(
            common,
            &[
                ("T.Writer.m1scr", "A = 1.0;\n"),
                ("T.Reader.m1scr", "B = A;\n"),
                ("T.Helper.m1scr", "Out = 1.0;\n"),
                (
                    "T.Orphan.m1scr",
                    "expand (I = 2 to 1)\n{\n\tA $(I) = 1.0;\n}\n",
                ),
            ],
        );
        let plan = build_schedule_plan(&loaded).expect("orphan helper must not block plan");
        assert_eq!(plan.dependencies.len(), 1);
        assert_eq!(plan.dependencies[0].writer, "Root.T.Writer");
    }
}
