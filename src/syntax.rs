// SPDX-License-Identifier: GPL-3.0-or-later
//! Reject parser recovery before executing any part of a supplied subtree.

use crate::EvalError;
use m1_core::Node;

pub(crate) fn validate(node: &Node, script: &str) -> Result<(), EvalError> {
    validate_mapped(node, script, None)
}

pub(crate) fn validate_scripts(
    scripts: &[m1_typecheck::parsed::ParsedScript],
) -> Result<(), EvalError> {
    for script in scripts {
        validate(&script.cst.root(), &script.name)?;
    }
    Ok(())
}

pub(crate) fn validate_mapped(
    node: &Node,
    script: &str,
    origin: Option<&[usize]>,
) -> Result<(), EvalError> {
    let mut pending = vec![(*node, 1)];
    while let Some((child, depth)) = pending.pop() {
        if depth > m1_core::MAX_RECURSION_DEPTH {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "source nesting exceeds {} levels in {script}",
                    m1_core::MAX_RECURSION_DEPTH
                ),
                at: origin.map_or(child.byte_range().start, |map| {
                    map[child.byte_range().start]
                }),
            });
        }
        if child.is_error() || child.is_missing() {
            let range = child.byte_range();
            let start = origin.map_or(range.start, |map| map[range.start]);
            let end = origin.map_or(range.end, |map| map[range.end]);
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "syntax error in {script} at bytes {}..{}: {} {:?}",
                    start,
                    end,
                    if child.is_missing() {
                        "missing"
                    } else {
                        "unexpected"
                    },
                    child.kind(),
                ),
                at: start,
            });
        }
        pending.extend(child.child_nodes().rev().map(|node| (node, depth + 1)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_limit_checks_actual_tree_depth() {
        for (depth, accepted) in [(128, true), (m1_core::MAX_RECURSION_DEPTH + 1, false)] {
            let source = format!("Output = {}1.0{};", "(".repeat(depth), ")".repeat(depth));
            let cst = m1_core::parse(&source);
            assert_eq!(validate(&cst.root(), "deep.m1scr").is_ok(), accepted);
        }
    }
}
