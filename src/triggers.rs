// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolve each script-backed function's effective project trigger.
//!
//! M1 projects may copy `SelectedTrigger` from another component with an
//! attribute expression such as
//! `$(Parent.Absolute Travel.Calculation:SelectedTrigger)`. The copied value can
//! itself be relative to that component. `m1-typecheck` deliberately leaves
//! these expressions without a `call_rate_hz`, so the evaluator resolves the
//! small project-attribute chain here before it builds any execution schedule.

use crate::error::EvalError;
use m1_typecheck::Project;
use m1_typecheck::parsed::ParsedScript;
use std::collections::{BTreeMap, BTreeSet};

/// The effective scheduling role of one script-backed project function.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerStatus {
    /// Run periodically at the resolved rate in Hz.
    Periodic(f64),
    /// Run once before the periodic loop.
    Startup,
    /// Callable function that is never an independent scheduler entry point.
    Helper,
    /// A schedulable function whose project component has no selected trigger.
    Unscheduled,
    /// A selected trigger exists, but cannot be resolved safely.
    Unresolved {
        /// The trigger expression declared on the script-backed function.
        trigger: String,
        /// A concrete explanation suitable for coverage output.
        reason: String,
    },
}

/// Resolved trigger state, keyed by canonical function symbol.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriggerMap {
    by_function: BTreeMap<String, TriggerStatus>,
}

impl TriggerMap {
    /// Resolve trigger state for every discovered script with a project symbol.
    pub(crate) fn from_project_xml(
        xml: &str,
        project: &Project,
        scripts: &[ParsedScript],
    ) -> Result<Self, EvalError> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| EvalError::UnsupportedConstruct {
            kind: format!("project XML re-parse for calculation triggers failed: {e}"),
            at: 0,
        })?;
        let components: BTreeMap<String, Component> = doc
            .descendants()
            .filter(|node| node.has_tag_name("Component"))
            .filter_map(|node| {
                let name = node.attribute("Name")?;
                let classname = node.attribute("Classname")?;
                let selected_trigger = node
                    .children()
                    .find(|child| child.has_tag_name("Props"))
                    .and_then(|props| props.attribute("SelectedTrigger"))
                    .map(str::to_string);
                Some((
                    name.to_string(),
                    Component {
                        classname: classname.to_string(),
                        selected_trigger,
                    },
                ))
            })
            .collect();

        let mut by_function = BTreeMap::new();
        for script in scripts {
            let Some(function) = project.function_symbol_for_script(&script.name) else {
                continue;
            };
            let status = match components.get(&function) {
                Some(component) if is_helper_class(&component.classname) => TriggerStatus::Helper,
                Some(component) => match component
                    .selected_trigger
                    .as_deref()
                    .map(str::trim)
                    .filter(|trigger| !trigger.is_empty())
                {
                    Some(trigger) => resolve_selected_trigger(
                        &function,
                        trigger,
                        trigger,
                        &components,
                        &mut BTreeSet::new(),
                    ),
                    None => TriggerStatus::Unscheduled,
                },
                None => TriggerStatus::Unresolved {
                    trigger: String::new(),
                    reason: "the script's function component is missing from Project.m1prj"
                        .to_string(),
                },
            };
            by_function.insert(function, status);
        }
        Ok(Self { by_function })
    }

    /// Trigger state for a canonical function symbol.
    pub fn get(&self, function: &str) -> Option<&TriggerStatus> {
        self.by_function.get(function)
    }

    /// Resolved periodic rate for a canonical function symbol.
    pub fn periodic_rate(&self, function: &str) -> Option<f64> {
        match self.get(function) {
            Some(TriggerStatus::Periodic(rate)) => Some(*rate),
            _ => None,
        }
    }

    /// Iterate over canonical function symbols and their trigger states.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &TriggerStatus)> {
        self.by_function
            .iter()
            .map(|(function, status)| (function.as_str(), status))
    }
}

#[derive(Debug, Clone)]
struct Component {
    classname: String,
    selected_trigger: Option<String>,
}

/// Parameterised user functions and calibration functions are invoked by other
/// code or tooling. M1 does not schedule them as independent event functions.
fn is_helper_class(classname: &str) -> bool {
    classname.starts_with("BuiltIn.FuncUserParam") || classname.starts_with("BuiltIn.CalFuncUser")
}

fn resolve_selected_trigger(
    owner: &str,
    selected: &str,
    original: &str,
    components: &BTreeMap<String, Component>,
    resolving: &mut BTreeSet<String>,
) -> TriggerStatus {
    let selected = selected.trim();
    if is_startup_trigger(selected) {
        return TriggerStatus::Startup;
    }

    if selected.starts_with("$(") {
        let Some((reference, attribute)) = parse_attribute_reference(selected) else {
            return unresolved(
                original,
                format!(
                    "invalid attribute expression `{selected}`; expected `$(<component>:SelectedTrigger)`"
                ),
            );
        };
        if attribute != "SelectedTrigger" {
            return unresolved(
                original,
                format!(
                    "attribute expression reads `{attribute}`; only `SelectedTrigger` can define a calculation rate"
                ),
            );
        }
        let Some(target) = resolve_component_path(owner, reference) else {
            return unresolved(
                original,
                format!("attribute reference `{reference}` climbs above the project root"),
            );
        };
        let Some(component) = components.get(&target) else {
            return unresolved(
                original,
                format!("attribute reference resolves to missing component `{target}`"),
            );
        };
        let Some(next) = component
            .selected_trigger
            .as_deref()
            .map(str::trim)
            .filter(|trigger| !trigger.is_empty())
        else {
            return unresolved(
                original,
                format!("referenced component `{target}` has no SelectedTrigger"),
            );
        };
        if !resolving.insert(target.clone()) {
            return unresolved(
                original,
                format!("SelectedTrigger attribute references form a cycle at `{target}`"),
            );
        }
        let status = resolve_selected_trigger(&target, next, original, components, resolving);
        resolving.remove(&target);
        return status;
    }

    if selected.contains("$(") {
        return unresolved(
            original,
            format!(
                "unsupported mixed trigger expression `{selected}`; use one event path or one SelectedTrigger reference"
            ),
        );
    }

    let Some(target) = resolve_component_path(owner, selected) else {
        return unresolved(
            original,
            format!("trigger path `{selected}` climbs above the project root"),
        );
    };
    let Some(component) = components.get(&target) else {
        return unresolved(
            original,
            format!("trigger resolves to missing component `{target}`"),
        );
    };
    if component.classname != "BuiltIn.EventKernel" {
        return unresolved(
            original,
            format!(
                "trigger resolves to `{target}` ({}) instead of a BuiltIn.EventKernel",
                component.classname
            ),
        );
    }
    let leaf = target.rsplit('.').next().unwrap_or(target.as_str());
    let Some(number) = leaf
        .strip_prefix("On ")
        .and_then(|value| value.strip_suffix("Hz"))
    else {
        return unresolved(
            original,
            format!("event kernel `{target}` does not name an `On <rate>Hz` event"),
        );
    };
    let Ok(rate) = number.trim().parse::<f64>() else {
        return unresolved(
            original,
            format!("event kernel `{target}` has an invalid numeric rate"),
        );
    };
    if !rate.is_finite() || rate <= 0.0 {
        return unresolved(
            original,
            format!("event kernel `{target}` must have a positive finite rate"),
        );
    }
    TriggerStatus::Periodic(rate)
}

fn unresolved(trigger: &str, reason: String) -> TriggerStatus {
    TriggerStatus::Unresolved {
        trigger: trigger.to_string(),
        reason,
    }
}

fn is_startup_trigger(trigger: &str) -> bool {
    trigger.eq_ignore_ascii_case("startup")
        || trigger.eq_ignore_ascii_case("On Startup")
        || trigger
            .rsplit('.')
            .next()
            .is_some_and(|leaf| leaf.eq_ignore_ascii_case("On Startup"))
}

fn parse_attribute_reference(expression: &str) -> Option<(&str, &str)> {
    let body = expression.strip_prefix("$(")?.strip_suffix(')')?;
    let (component, attribute) = body.rsplit_once(':')?;
    let component = component.trim();
    let attribute = attribute.trim();
    (!component.is_empty() && !attribute.is_empty()).then_some((component, attribute))
}

/// Resolve M1's component-relative path form. `Parent.` starts at the owner
/// component itself, so one `Parent` reaches its enclosing group. Absolute
/// `Root` paths pass through unchanged.
fn resolve_component_path(owner: &str, value: &str) -> Option<String> {
    let value = value.trim();
    if value == "Root" || value.starts_with("Root.") {
        return Some(value.to_string());
    }
    let owner_segments: Vec<&str> = owner.split('.').collect();
    let mut climb = 0usize;
    let mut rest = value;
    while let Some(tail) = rest.strip_prefix("Parent.") {
        climb += 1;
        rest = tail;
    }
    if climb > owner_segments.len() || rest.is_empty() {
        return None;
    }
    let ancestor = &owner_segments[..owner_segments.len() - climb];
    if ancestor.is_empty() {
        Some(rest.to_string())
    } else {
        Some(format!("{}.{rest}", ancestor.join(".")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use m1_typecheck::parsed::parse_all;

    fn statuses(xml: &str, files: &[&str]) -> TriggerMap {
        let project = Project::from_xml(xml).expect("project parses");
        let sources: Vec<(String, String)> = files
            .iter()
            .map(|file| ((*file).to_string(), String::new()))
            .collect();
        let scripts = parse_all(&sources);
        TriggerMap::from_project_xml(xml, &project, &scripts).expect("triggers resolve")
    }

    #[test]
    fn resolves_direct_grouped_parameterized_and_nonperiodic_statuses() {
        let xml = r#"<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Events"/>
  <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 100Hz"/>
  <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 200Hz"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Direct" Filename="Direct.m1scr"><Props SelectedTrigger="Root.Events.On 100Hz"/></Component>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Control.Grouped" Filename="Grouped.m1scr"><Props SelectedTrigger="Parent.Parent.Events.On 100Hz"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.Calculation"><Props SelectedTrigger="Parent.Parent.Events.On 200Hz"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Control.Parameterized" Filename="Parameterized.m1scr"><Props SelectedTrigger="$(Parent.Calculation:SelectedTrigger)"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Startup" Filename="Startup.m1scr"><Props SelectedTrigger="Parent.Events.On Startup"/></Component>
  <Component Classname="BuiltIn.FuncUserParam" Name="Root.Helper" Filename="Helper.m1scr"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Unscheduled" Filename="Unscheduled.m1scr"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Invalid" Filename="Invalid.m1scr"><Props SelectedTrigger="Root.Events.On 999Hz"/></Component>
</Project>"#;
        let map = statuses(
            xml,
            &[
                "Direct.m1scr",
                "Grouped.m1scr",
                "Parameterized.m1scr",
                "Startup.m1scr",
                "Helper.m1scr",
                "Unscheduled.m1scr",
                "Invalid.m1scr",
            ],
        );

        assert_eq!(
            map.get("Root.Direct"),
            Some(&TriggerStatus::Periodic(100.0))
        );
        assert_eq!(
            map.get("Root.Control.Grouped"),
            Some(&TriggerStatus::Periodic(100.0))
        );
        assert_eq!(
            map.get("Root.Control.Parameterized"),
            Some(&TriggerStatus::Periodic(200.0))
        );
        assert_eq!(map.get("Root.Startup"), Some(&TriggerStatus::Startup));
        assert_eq!(map.get("Root.Helper"), Some(&TriggerStatus::Helper));
        assert_eq!(
            map.get("Root.Unscheduled"),
            Some(&TriggerStatus::Unscheduled)
        );
        assert!(matches!(
            map.get("Root.Invalid"),
            Some(TriggerStatus::Unresolved { reason, .. })
                if reason.contains("missing component `Root.Events.On 999Hz`")
        ));
    }

    #[test]
    fn reports_missing_selected_trigger_reference_and_cycles() {
        let xml = r#"<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.A"><Props SelectedTrigger="$(Root.B:SelectedTrigger)"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.B"><Props SelectedTrigger="$(Root.A:SelectedTrigger)"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Cycle" Filename="Cycle.m1scr"><Props SelectedTrigger="$(Root.A:SelectedTrigger)"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Missing" Filename="Missing.m1scr"><Props SelectedTrigger="$(Root.Nope:SelectedTrigger)"/></Component>
</Project>"#;
        let map = statuses(xml, &["Cycle.m1scr", "Missing.m1scr"]);
        assert!(matches!(
            map.get("Root.Cycle"),
            Some(TriggerStatus::Unresolved { reason, .. }) if reason.contains("form a cycle")
        ));
        assert!(matches!(
            map.get("Root.Missing"),
            Some(TriggerStatus::Unresolved { reason, .. }) if reason.contains("missing component")
        ));
    }
}
