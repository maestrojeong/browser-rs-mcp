//! Accessibility-tree snapshot: the representation an agent actually reasons over.
//!
//! We pull the full AX tree (`Accessibility.getFullAXTree`) and render it as a
//! compact indented outline. Interactive nodes get stable `[ref=eN]` handles so
//! the agent can act on them by reference instead of brittle CSS selectors.
//!
//! Design goal: minimal tokens, maximum signal. Ignored/empty nodes are pruned.

use std::collections::HashMap;

use serde_json::Value;

/// A rendered snapshot plus the ref -> backendDOMNodeId map used by act tools.
pub struct Snapshot {
    pub text: String,
    /// ref id (e.g. "e3") -> backendDOMNodeId
    pub refs: HashMap<String, i64>,
}

/// Roles that are interactive enough to warrant a [ref].
fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "switch"
            | "slider"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "option"
            | "spinbutton"
    )
}

/// Roles that carry no useful structure on their own and can be flattened out.
fn is_noise(role: &str) -> bool {
    matches!(
        role,
        "none" | "generic" | "InlineTextBox" | "" | "presentation"
    )
}

/// AXValue is `{ "type": ..., "value": <string> }`; the payload is one level in.
fn str_prop(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Build a snapshot from the `nodes` array returned by Accessibility.getFullAXTree.
pub fn render(nodes: &[Value]) -> Snapshot {
    // Index nodes by their AX nodeId and remember child order.
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    let mut root_id: Option<&str> = None;
    for n in nodes {
        if let Some(id) = n.get("nodeId").and_then(Value::as_str) {
            by_id.insert(id, n);
            if root_id.is_none() && n.get("parentId").is_none() {
                root_id = Some(id);
            }
        }
    }
    // Fallback: first node is root if none had a missing parent.
    if root_id.is_none() {
        root_id = nodes
            .first()
            .and_then(|n| n.get("nodeId"))
            .and_then(Value::as_str);
    }

    let mut out = String::new();
    let mut refs = HashMap::new();
    let mut counter = 0u32;

    if let Some(rid) = root_id {
        walk(rid, &by_id, 0, &mut out, &mut refs, &mut counter);
    }

    Snapshot { text: out, refs }
}

#[allow(clippy::only_used_in_recursion)]
fn walk(
    id: &str,
    by_id: &HashMap<&str, &Value>,
    depth: usize,
    out: &mut String,
    refs: &mut HashMap<String, i64>,
    counter: &mut u32,
) {
    let Some(node) = by_id.get(id) else { return };

    let ignored = node
        .get("ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let role = node
        .get("role")
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = str_prop(node, "name");

    // Decide whether this node earns a printed line.
    let printable = !ignored && !is_noise(role) && (!name.is_empty() || is_interactive(role));

    let child_depth = if printable {
        let indent = "  ".repeat(depth);
        let mut line = format!("{indent}{role}");
        if !name.is_empty() {
            line.push_str(&format!(" \"{}\"", truncate(&name, 120)));
        }
        if is_interactive(role) {
            *counter += 1;
            let r = format!("e{}", *counter);
            if let Some(backend) = node.get("backendDOMNodeId").and_then(Value::as_i64) {
                refs.insert(r.clone(), backend);
            }
            line.push_str(&format!(" [ref={r}]"));
        }
        out.push_str(&line);
        out.push('\n');
        depth + 1
    } else {
        depth
    };

    if let Some(children) = node.get("childIds").and_then(Value::as_array) {
        for c in children {
            if let Some(cid) = c.as_str() {
                walk(cid, by_id, child_depth, out, refs, counter);
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
