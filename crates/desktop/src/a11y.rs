//! Screen readers, through AccessKit.
//!
//! The UI is immediate mode, so there is no widget tree to hand over.
//! Instead, while a screen reader is connected, widgets describe themselves
//! to [`Ui`](crate::ui::Ui) as they draw, and after the frame that list
//! becomes the tree: the window holding one node per widget, in drawing
//! order. Without a screen reader nothing is collected or built, and a frame
//! whose list did not change sends nothing.
//!
//! ponytail: Windows and macOS only. Linux's AT-SPI needs `accesskit_unix`,
//! which brings zbus and an async executor (about 65 crates); add it if Linux
//! screen reader users ask. The tree is flat (no groups or headings) and the
//! only action is Click, delivered as a click at the widget's centre.

use std::collections::HashMap;

use accesskit::{Action, ActionRequest, Node as AkNode, NodeId, Rect as AkRect, Role, Toggled, TreeId, TreeInfo, TreeUpdate};
use accesskit_winit::Adapter;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::Window;

use crate::app::WorkerEvent;
use crate::paint::Rect;
use crate::ui::id;

/// The window itself.
const ROOT: NodeId = NodeId(0);

/// One widget as screen readers see it.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    role: Role,
    label: String,
    rect: Rect,
    /// A text field's contents.
    value: Option<String>,
    /// A switch's or choice's state.
    toggled: Option<bool>,
    /// Has keyboard focus (where typing goes).
    focused: bool,
}

impl Node {
    /// A widget of `role` named `label`, drawn at `rect` (logical pixels).
    #[must_use]
    pub fn new(role: Role, rect: Rect, label: &str) -> Self {
        Self { role, label: label.to_owned(), rect, value: None, toggled: None, focused: false }
    }

    /// A text field holding `value`; `focused` if typing goes to it.
    #[must_use]
    pub fn input(rect: Rect, label: &str, value: &str, multiline: bool, focused: bool) -> Self {
        let role = if multiline { Role::MultilineTextInput } else { Role::TextInput };
        Self { value: Some(value.to_owned()), focused, ..Self::new(role, rect, label) }
    }

    /// An on/off switch.
    #[must_use]
    pub fn switch(rect: Rect, label: &str, on: bool) -> Self {
        Self { toggled: Some(on), ..Self::new(Role::Switch, rect, label) }
    }

    /// One of several choices (a radio button, or a tab when `tab`).
    #[must_use]
    pub fn choice(rect: Rect, label: &str, selected: bool, tab: bool) -> Self {
        let role = if tab { Role::Tab } else { Role::RadioButton };
        Self { toggled: Some(selected), ..Self::new(role, rect, label) }
    }

    fn clickable(&self) -> bool {
        matches!(self.role, Role::Button | Role::Switch | Role::RadioButton | Role::Tab | Role::MenuItem)
    }
}

/// Ids for `nodes`: stable across frames as long as a widget keeps its role
/// and label, so the screen reader keeps its place while things redraw.
fn ids(nodes: &[Node]) -> Vec<NodeId> {
    let mut seen: HashMap<u64, u32> = HashMap::new();
    nodes
        .iter()
        .map(|n| {
            let base = id((n.role as u8, &n.label));
            let nth = seen.entry(base).or_default();
            *nth += 1;
            // Never 0, which is the root.
            NodeId(id((base, *nth)).max(1))
        })
        .collect()
}

/// The whole tree for `nodes`, with bounds in physical pixels.
fn tree(nodes: &[Node], ids: &[NodeId], scale: f64) -> TreeUpdate {
    let mut root = AkNode::new(Role::Window);
    root.set_label("SereChat");
    root.set_children(ids.to_vec());
    let mut out = Vec::with_capacity(nodes.len() + 1);
    out.push((ROOT, root));
    let mut focus = ROOT;
    for (node, &node_id) in nodes.iter().zip(ids) {
        let mut ak = AkNode::new(node.role);
        ak.set_label(node.label.as_str());
        let r = node.rect;
        let (x0, y0) = (f64::from(r.x) * scale, f64::from(r.y) * scale);
        ak.set_bounds(AkRect { x0, y0, x1: x0 + f64::from(r.w) * scale, y1: y0 + f64::from(r.h) * scale });
        if let Some(value) = &node.value {
            ak.set_value(value.as_str());
        }
        match node.toggled {
            // A tab is selected rather than checked.
            Some(on) if node.role == Role::Tab => ak.set_selected(on),
            Some(on) => ak.set_toggled(if on { Toggled::True } else { Toggled::False }),
            None => {}
        }
        if node.clickable() {
            ak.add_action(Action::Click);
        }
        if node.focused {
            ak.add_action(Action::Focus);
            focus = node_id;
        }
        out.push((node_id, ak));
    }
    TreeUpdate { nodes: out, tree: Some(TreeInfo::new(ROOT)), tree_id: TreeId::ROOT, focus }
}

/// The window's connection to screen readers.
pub struct Access {
    adapter: Adapter,
    /// A screen reader is connected: widgets should describe themselves.
    active: bool,
    /// What was sent last, to skip unchanged frames and find click targets.
    sent: Vec<Node>,
    sent_ids: Vec<NodeId>,
}

impl Access {
    /// Connects `window`, which must not have been shown yet.
    pub fn new(event_loop: &ActiveEventLoop, window: &Window, proxy: EventLoopProxy<WorkerEvent>) -> Self {
        Self { adapter: Adapter::with_event_loop_proxy(event_loop, window, proxy), active: false, sent: Vec::new(), sent_ids: Vec::new() }
    }

    /// Whether widgets should describe themselves this frame.
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// Lets the adapter see every window event first (focus, resizes).
    pub fn window_event(&mut self, window: &Window, event: &WindowEvent) {
        self.adapter.process_event(window, event);
    }

    /// Handles a screen reader request. Returns the point (logical pixels)
    /// to click for a Click request, and whether a redraw is needed.
    pub fn event(&mut self, event: &accesskit_winit::WindowEvent) -> (Option<(f32, f32)>, bool) {
        match event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.active = true;
                // Forces a full tree out on the next frame.
                self.sent.clear();
                (None, true)
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.active = false;
                self.sent.clear();
                (None, false)
            }
            accesskit_winit::WindowEvent::ActionRequested(ActionRequest { action: Action::Click, target_node, .. }) => {
                let target = self.sent_ids.iter().position(|n| n == target_node).map(|i| self.sent[i].rect);
                (target.map(|r| (r.x + r.w * 0.5, r.y + r.h * 0.5)), true)
            }
            accesskit_winit::WindowEvent::ActionRequested(_) => (None, false),
        }
    }

    /// Sends the frame's widgets, unless they are what was sent last.
    pub fn update(&mut self, nodes: Vec<Node>, scale: f64) {
        if !self.active || nodes == self.sent {
            return;
        }
        let ids = ids(&nodes);
        self.adapter.update_if_active(|| tree(&nodes, &ids, scale));
        self.sent = nodes;
        self.sent_ids = ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        let r = Rect::default();
        let nodes = [Node::new(Role::Button, r, "Copy"), Node::new(Role::Button, r, "Copy"), Node::new(Role::Label, r, "Copy")];
        let first = ids(&nodes);
        assert_eq!(first, ids(&nodes), "the same frame gets the same ids");
        assert!(first[0] != first[1] && first[0] != first[2] && first[1] != first[2]);
        assert!(first.iter().all(|&n| n != ROOT));
        // A widget's id doesn't depend on where it is drawn.
        let moved = [Node::new(Role::Button, Rect::new(5.0, 5.0, 1.0, 1.0), "Copy")];
        assert_eq!(ids(&moved)[0], first[0]);
    }

    #[test]
    fn the_tree_has_focus_bounds_and_actions() {
        let nodes = [
            Node::new(Role::Button, Rect::new(10.0, 20.0, 30.0, 40.0), "Send"),
            Node::input(Rect::default(), "Message", "hi", true, true),
            Node::switch(Rect::default(), "Updates", true),
        ];
        let ids = ids(&nodes);
        let update = tree(&nodes, &ids, 2.0);
        assert_eq!(update.focus, ids[1]);
        assert_eq!(update.nodes.len(), 4);
        let (_, send) = &update.nodes[1];
        assert_eq!(send.bounds(), Some(AkRect { x0: 20.0, y0: 40.0, x1: 80.0, y1: 120.0 }));
        assert!(send.supports_action(Action::Click));
        assert_eq!(update.nodes[2].1.value(), Some("hi"));
        assert_eq!(update.nodes[3].1.toggled(), Some(Toggled::True));
        assert_eq!(update.nodes[0].1.children(), ids.as_slice());
    }
}
