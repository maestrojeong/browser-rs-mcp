//! Accessibility-tree snapshot: the representation an agent actually reasons over.
//!
//! We pull the full AX tree (`Accessibility.getFullAXTree`) and render it as a
//! compact indented outline. Interactive nodes get stable `[ref=eN]` handles so
//! the agent can act on them by reference instead of brittle CSS selectors.
//!
//! Design goal: minimal tokens, maximum signal. Ignored/empty nodes are pruned.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// A rendered snapshot plus the ref -> backendDOMNodeId map used by act tools.
pub struct Snapshot {
    pub text: String,
    /// Snapshot-scoped ref id -> live-node capability.
    pub refs: HashMap<String, ElementRef>,
}

/// Main-frame document identity captured when an accessibility ref is minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentIdentity {
    pub target_id: String,
    pub frame_id: String,
    pub loader_id: String,
}

/// A snapshot-scoped element capability. The backend id is never sufficient by
/// itself: callers must re-prove `document` before mutating the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementRef {
    pub backend_node_id: i64,
    pub document: Option<DocumentIdentity>,
}

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

struct RenderContext<'a> {
    snapshot_id: u64,
    document: Option<&'a DocumentIdentity>,
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

/// Iframe roles. These are surfaced even with no accessible name and no
/// children, because a same-process (same-origin) iframe's content is
/// already flattened into this same tree, while a cross-process
/// (cross-origin) iframe's content is invisible to `Accessibility.getFullAXTree`
/// entirely (separate renderer, separate CDP target) — without this line the
/// agent would have no signal that an iframe exists at all. Use
/// `browser_iframe_read` / `browser_iframe_click` / `browser_iframe_type` to
/// reach inside it.
fn is_iframe(role: &str) -> bool {
    matches!(role, "Iframe" | "iframe" | "IframePresentational")
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
    render_with_document(nodes, None)
}

pub(crate) fn render_with_document(
    nodes: &[Value],
    document: Option<DocumentIdentity>,
) -> Snapshot {
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
    let context = RenderContext {
        snapshot_id: NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed),
        document: document.as_ref(),
    };

    if let Some(rid) = root_id {
        walk(rid, &by_id, 0, &mut out, &mut refs, &mut counter, &context);
    }

    Snapshot { text: out, refs }
}

#[allow(clippy::only_used_in_recursion)]
fn walk(
    id: &str,
    by_id: &HashMap<&str, &Value>,
    depth: usize,
    out: &mut String,
    refs: &mut HashMap<String, ElementRef>,
    counter: &mut u32,
    context: &RenderContext<'_>,
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

    // Decide whether this node earns a printed line. Iframes are always
    // printed (even nameless, childless ones) — see `is_iframe`.
    let printable = !ignored
        && !is_noise(role)
        && (!name.is_empty() || is_interactive(role) || is_iframe(role));

    let child_depth = if printable {
        let indent = "  ".repeat(depth);
        let mut line = format!("{indent}{role}");
        if !name.is_empty() {
            line.push_str(&format!(" \"{}\"", truncate(&name, 120)));
        }
        if is_iframe(role) {
            line.push_str(" [iframe: use browser_iframe_read/_click/_fill]");
        }
        if is_interactive(role) {
            *counter += 1;
            let r = format!("e{}_{}", context.snapshot_id, *counter);
            if let Some(backend) = node.get("backendDOMNodeId").and_then(Value::as_i64) {
                refs.insert(
                    r.clone(),
                    ElementRef {
                        backend_node_id: backend,
                        document: context.document.cloned(),
                    },
                );
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
                walk(cid, by_id, child_depth, out, refs, counter, context);
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

#[cfg(test)]
mod tests {
    use super::{render, render_with_document, DocumentIdentity};
    use serde_json::json;

    #[test]
    fn nameless_childless_iframe_is_still_printed() {
        // A cross-origin OOPIF typically shows up as a nameless, childless
        // "Iframe" node — regression check that it isn't silently dropped
        // the way a nameless "generic"/"none" node would be.
        let nodes = vec![
            json!({
                "nodeId": "1",
                "role": {"value": "RootWebArea"},
                "name": {"value": "page"},
                "childIds": ["2"]
            }),
            json!({
                "nodeId": "2",
                "role": {"value": "Iframe"},
                "name": {"value": ""}
            }),
        ];
        let snap = render(&nodes);
        assert!(
            snap.text.contains("Iframe"),
            "expected iframe node in snapshot text, got: {}",
            snap.text
        );
        assert!(snap.text.contains("browser_iframe_read"));
    }

    #[test]
    fn iframe_does_not_get_a_click_ref() {
        // Iframes aren't in `is_interactive`, so they must not consume a
        // [ref=eN] slot (that's reserved for click/type/etc. targets).
        let nodes = vec![
            json!({
                "nodeId": "1",
                "role": {"value": "RootWebArea"},
                "name": {"value": "page"},
                "childIds": ["2"]
            }),
            json!({
                "nodeId": "2",
                "role": {"value": "Iframe"},
                "name": {"value": ""}
            }),
        ];
        let snap = render(&nodes);
        assert!(snap.refs.is_empty());
        assert!(!snap.text.contains("[ref="));
    }

    #[test]
    fn nameless_generic_node_is_still_pruned() {
        // Regression guard: the iframe carve-out must not accidentally
        // widen visibility for ordinary structural noise.
        let nodes = vec![
            json!({
                "nodeId": "1",
                "role": {"value": "RootWebArea"},
                "name": {"value": "page"},
                "childIds": ["2"]
            }),
            json!({
                "nodeId": "2",
                "role": {"value": "generic"},
                "name": {"value": ""}
            }),
        ];
        let snap = render(&nodes);
        assert!(!snap.text.contains("generic"));
    }

    #[test]
    fn refs_are_unique_across_snapshots_and_carry_document_identity() {
        let nodes = vec![json!({
            "nodeId": "1",
            "role": {"value": "button"},
            "name": {"value": "Save"},
            "backendDOMNodeId": 42
        })];
        let identity = DocumentIdentity {
            target_id: "target".into(),
            frame_id: "frame".into(),
            loader_id: "loader".into(),
        };
        let first = render_with_document(&nodes, Some(identity.clone()));
        let second = render_with_document(&nodes, Some(identity.clone()));
        let first_ref = first.refs.iter().next().unwrap();
        let second_ref = second.refs.iter().next().unwrap();
        assert_ne!(first_ref.0, second_ref.0);
        assert_eq!(first_ref.1.backend_node_id, 42);
        assert_eq!(first_ref.1.document.as_ref(), Some(&identity));
    }
}
