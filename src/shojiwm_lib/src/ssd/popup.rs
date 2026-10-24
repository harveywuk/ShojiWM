//! `<Popup>`: a decoration subtree anchored to its parent node but drawn
//! outside the window (tooltips, hover labels).
//!
//! A popup is laid out with the rest of the tree, so node ids, signals,
//! paint shaders and uniform patches work as everywhere else. What differs:
//!
//! - it takes no space in its parent's flow and is sized by its content;
//! - it is placed next to its parent (the anchor) and flipped to the other
//!   side when it would leave the output ([`set_popup_viewports`]);
//! - it ignores the clips of its ancestors;
//! - the backends draw it in its own pass ([`PopupScopes`]): above every
//!   window ([`PopupLayer::Top`]) or right above its own window
//!   ([`PopupLayer::Window`]), never inside the window's element stream;
//! - its [`PopupMode`] decides input, like the HTML `popover` attribute: a
//!   `Hint` lets pointer input through, `Auto` and `Manual` popups take it
//!   (above the clients they cover), and the compositor asks an `Auto` popup
//!   to close on a press outside, Escape, when its window goes away, or when
//!   another `Auto` popup opens ([`PopupDismissReason`]);
//! - while the pointer is inside an interactive popup, the popup's ancestors
//!   count as hovered too (DOM `:hover`), so `open={hover}` on the anchor
//!   keeps a menu open while the pointer is on it.
//!
//! The open state stays with the config: the compositor only reports what
//! happened through the [`PopupHandlers`].

use std::cell::RefCell;

use super::{
    ComputedDecorationNode, DecorationInteractionTarget, DecorationNodeKind,
    DecorationStateChangeHandler, LayoutFrame, LayoutPoint, LogicalRect, ResolvedLayoutValue,
    ResolvedLogicalRect,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupNode {
    /// Closed popups are laid out but neither drawn nor hit.
    pub open: bool,
    pub placement: PopupPlacement,
    pub align: PopupAlign,
    /// Distance from the anchor in logical pixels.
    pub offset: f64,
    pub collision: PopupCollision,
    pub layer: PopupLayer,
    pub mode: PopupMode,
    /// `Auto` only: Escape asks the popup to close (and does not reach the
    /// focused client while the popup is open).
    pub close_on_escape: bool,
    /// `Auto` only: a press outside the popup and its anchor asks it to close.
    pub close_on_outside_press: bool,
    /// Interactive only: a press on the popup away from its buttons moves its
    /// window, like a title bar (a bar drawn over the client).
    pub moves_window: bool,
}

impl Default for PopupNode {
    fn default() -> Self {
        Self {
            open: true,
            placement: PopupPlacement::Bottom,
            align: PopupAlign::Center,
            offset: 0.0,
            collision: PopupCollision::Flip,
            layer: PopupLayer::Top,
            mode: PopupMode::Hint,
            close_on_escape: true,
            close_on_outside_press: true,
            moves_window: false,
        }
    }
}

/// How a popup takes part in input (the HTML `popover` attribute).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupMode {
    /// A tooltip: pointer input falls through, nothing closes it.
    #[default]
    Hint,
    /// Takes pointer input and is light-dismissed (see [`PopupDismissReason`]).
    Auto,
    /// Takes pointer input; only the config closes it.
    Manual,
}

impl PopupMode {
    pub fn is_interactive(self) -> bool {
        !matches!(self, Self::Hint)
    }
}

/// Why the compositor asks an `Auto` popup to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PopupDismissReason {
    /// A pointer press outside the popup (and its nested popups) and outside
    /// its anchor.
    OutsidePress,
    /// Escape, while it is the most recently opened `Auto` popup.
    Escape,
    /// Its window was hidden (minimized, another workspace, ...).
    AnchorGone,
    /// Another `Auto` popup that is not nested in it opened.
    OtherPopup,
}

impl PopupDismissReason {
    pub const ALL: [Self; 4] = [
        Self::OutsidePress,
        Self::Escape,
        Self::AnchorGone,
        Self::OtherPopup,
    ];

    /// The name the config runtimes see (`onOpenChange(false, reason)`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutsidePress => "outside-press",
            Self::Escape => "escape",
            Self::AnchorGone => "anchor-gone",
            Self::OtherPopup => "other-popup",
        }
    }
}

/// What a popup node wants to hear about. Handler ids are invoked like any
/// other decoration handler.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PopupHandlers {
    /// The pointer entered / left the anchor or, for an interactive popup,
    /// the popup itself (`interestfor`; drives `trigger="hover"`).
    pub interest_change: Option<DecorationStateChangeHandler>,
    /// A pointer press on the anchor (drives `trigger="click"`).
    pub anchor_press: Option<String>,
    /// Close requests of an `Auto` popup, one handler per reason
    /// (`onOpenChange`).
    pub dismiss: Option<PopupDismissHandlers>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupDismissHandlers {
    pub outside_press: String,
    pub escape: String,
    pub anchor_gone: String,
    pub other_popup: String,
}

impl PopupDismissHandlers {
    pub fn handler_for(&self, reason: PopupDismissReason) -> &str {
        match reason {
            PopupDismissReason::OutsidePress => &self.outside_press,
            PopupDismissReason::Escape => &self.escape,
            PopupDismissReason::AnchorGone => &self.anchor_gone,
            PopupDismissReason::OtherPopup => &self.other_popup,
        }
    }
}

/// The side of the anchor the popup goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupPlacement {
    Top,
    #[default]
    Bottom,
    Left,
    Right,
}

/// Where along that side the popup is aligned with the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupAlign {
    Start,
    #[default]
    Center,
    End,
}

/// What happens when the popup would leave the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupCollision {
    /// Go to the opposite side if it has more room, and slide along the side
    /// to stay on the output.
    #[default]
    Flip,
    /// Stay where `placement` and `align` put it.
    None,
}

/// Where the popup sits in the stacking order of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupLayer {
    /// Above every window, below layer-shell `top` / `overlay` surfaces.
    #[default]
    Top,
    /// Right above its own window: windows stacked higher cover it.
    Window,
}

thread_local! {
    static POPUP_VIEWPORTS: RefCell<Vec<LogicalRect>> = const { RefCell::new(Vec::new()) };
}

/// The output rects (global logical coordinates) popups are kept inside.
/// Updated by the compositor whenever it refreshes decorations; layout runs on
/// the same thread.
pub fn set_popup_viewports(viewports: Vec<LogicalRect>) {
    POPUP_VIEWPORTS.with(|slot| *slot.borrow_mut() = viewports);
}

/// The viewport of the output under the anchor's center, in the root-local
/// pixels of `frame`.
fn viewport_px(frame: LayoutFrame, anchor: ResolvedLogicalRect) -> Option<ResolvedLogicalRect> {
    let center_x = frame.logical_x(anchor.x) + frame.logical_len(anchor.width) as f64 / 2.0;
    let center_y = frame.logical_y(anchor.y) + frame.logical_len(anchor.height) as f64 / 2.0;
    let viewport = POPUP_VIEWPORTS.with(|slot| {
        let viewports = slot.borrow();
        let contains = |rect: &&LogicalRect| {
            center_x >= rect.x as f64
                && center_y >= rect.y as f64
                && center_x < (rect.x + rect.width) as f64
                && center_y < (rect.y + rect.height) as f64
        };
        viewports.iter().find(contains).copied()
    })?;
    let to_px = |value: f64| super::round_half_up(value);
    let top_left = frame.layout_point(viewport.x as f64, viewport.y as f64);
    let bottom_right = frame.layout_point(
        (viewport.x + viewport.width) as f64,
        (viewport.y + viewport.height) as f64,
    );
    Some(ResolvedLogicalRect::from_px(
        to_px(top_left.x),
        to_px(top_left.y),
        to_px(bottom_right.x) - to_px(top_left.x),
        to_px(bottom_right.y) - to_px(top_left.y),
    ))
}

/// The border box of a popup of `size` next to `anchor`.
pub(super) fn popup_rect(
    popup: &PopupNode,
    anchor: ResolvedLogicalRect,
    size: (ResolvedLayoutValue, ResolvedLayoutValue),
    frame: LayoutFrame,
) -> ResolvedLogicalRect {
    let viewport = match popup.collision {
        PopupCollision::Flip => viewport_px(frame, anchor),
        PopupCollision::None => None,
    };
    place(
        popup,
        anchor,
        size,
        ResolvedLayoutValue::from_logical(popup.offset, frame.scale),
        viewport,
    )
}

fn place(
    popup: &PopupNode,
    anchor: ResolvedLogicalRect,
    (width, height): (ResolvedLayoutValue, ResolvedLayoutValue),
    offset: ResolvedLayoutValue,
    viewport: Option<ResolvedLogicalRect>,
) -> ResolvedLogicalRect {
    let (w, h, offset) = (width.raw(), height.raw(), offset.raw());
    let (ax, ay, aw, ah) = (
        anchor.x.raw(),
        anchor.y.raw(),
        anchor.width.raw(),
        anchor.height.raw(),
    );
    let vertical = matches!(
        popup.placement,
        PopupPlacement::Top | PopupPlacement::Bottom
    );

    // Main axis: the side of the anchor.
    let before = |len: i32, start: i32| start - offset - len;
    let after = |start: i32, anchor_len: i32| start + anchor_len + offset;
    let (anchor_main, anchor_main_len, len) = if vertical { (ay, ah, h) } else { (ax, aw, w) };
    let on_before = matches!(popup.placement, PopupPlacement::Top | PopupPlacement::Left);
    let mut main = if on_before {
        before(len, anchor_main)
    } else {
        after(anchor_main, anchor_main_len)
    };
    if let Some(viewport) = viewport {
        let (view_start, view_len) = if vertical {
            (viewport.y.raw(), viewport.height.raw())
        } else {
            (viewport.x.raw(), viewport.width.raw())
        };
        let view_end = view_start + view_len;
        let room_before = anchor_main - offset - view_start;
        let room_after = view_end - (anchor_main + anchor_main_len + offset);
        let (room, other_room) = if on_before {
            (room_before, room_after)
        } else {
            (room_after, room_before)
        };
        if room < len && other_room > room {
            main = if on_before {
                after(anchor_main, anchor_main_len)
            } else {
                before(len, anchor_main)
            };
        }
    }

    // Cross axis: alignment with the anchor.
    let (anchor_cross, anchor_cross_len, cross_len) =
        if vertical { (ax, aw, w) } else { (ay, ah, h) };
    let mut cross = match popup.align {
        PopupAlign::Start => anchor_cross,
        PopupAlign::Center => anchor_cross + (anchor_cross_len - cross_len).div_euclid(2),
        PopupAlign::End => anchor_cross + anchor_cross_len - cross_len,
    };
    if let Some(viewport) = viewport {
        let (view_start, view_len) = if vertical {
            (viewport.x.raw(), viewport.width.raw())
        } else {
            (viewport.y.raw(), viewport.height.raw())
        };
        cross = cross.min(view_start + view_len - cross_len).max(view_start);
    }

    let (x, y) = if vertical {
        (cross, main)
    } else {
        (main, cross)
    };
    ResolvedLogicalRect::from_px(x, y, w, h)
}

/// What a backend draws in one pass over a window's decoration buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationPart {
    /// Everything but the popups: the window's own element stream.
    Window,
    /// The open popups of one layer.
    Popups(PopupLayer),
}

/// The open popups of a computed tree, by the buffer path prefix their paint,
/// text and icon buffers carry (`root/child-0/child-2`, see
/// `ssd::integration`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PopupScopes {
    scopes: Vec<(String, PopupLayer)>,
}

impl PopupScopes {
    pub fn of(root: &ComputedDecorationNode) -> Self {
        let mut scopes = Vec::new();
        // The common case, no open popup, allocates nothing.
        if has_open_popup(root) {
            collect_scopes(root, "root".to_string(), &mut scopes);
        }
        Self { scopes }
    }

    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    pub fn has_layer(&self, layer: PopupLayer) -> bool {
        self.scopes.iter().any(|(_, candidate)| *candidate == layer)
    }

    /// The layer of the popup a buffer `stable_key` belongs to.
    pub fn layer_of(&self, stable_key: &str) -> Option<PopupLayer> {
        self.scopes.iter().find_map(|(path, layer)| {
            let rest = stable_key.strip_prefix(path.as_str())?;
            (rest.starts_with(':') || rest.starts_with('/')).then_some(*layer)
        })
    }

    pub fn includes(&self, part: DecorationPart, stable_key: &str) -> bool {
        match part {
            DecorationPart::Window => self.is_empty() || self.layer_of(stable_key).is_none(),
            DecorationPart::Popups(layer) => self.layer_of(stable_key) == Some(layer),
        }
    }
}

fn has_open_popup(node: &ComputedDecorationNode) -> bool {
    node.style.visible != Some(false)
        && (matches!(node.kind, DecorationNodeKind::Popup(_))
            || node.children.iter().any(has_open_popup))
}

fn collect_scopes(
    node: &ComputedDecorationNode,
    path: String,
    scopes: &mut Vec<(String, PopupLayer)>,
) {
    if node.style.visible == Some(false) {
        return;
    }
    if let DecorationNodeKind::Popup(popup) = &node.kind {
        // A popup inside a popup belongs to the outer one's pass.
        scopes.push((path, popup.layer));
        return;
    }
    for (index, child) in node.children.iter().enumerate() {
        collect_scopes(child, format!("{path}/child-{index}"), scopes);
    }
}

/// A popup of a computed tree, as the compositor's input handling sees it:
/// what it is, where it and its anchor are, and what it wants to hear about.
#[derive(Debug, Clone)]
pub struct PopupInfo {
    pub node_id: Option<String>,
    pub popup: PopupNode,
    pub handlers: Option<PopupHandlers>,
    /// Ids of the popups it is nested in, outermost first.
    pub enclosing: Vec<String>,
    frame: LayoutFrame,
    anchor: ResolvedLogicalRect,
    /// Its own box and those of the open interactive popups nested in it.
    areas: Vec<ResolvedLogicalRect>,
}

impl PopupInfo {
    /// Drawn and taking pointer input.
    pub fn is_open_and_interactive(&self) -> bool {
        self.popup.open && self.popup.mode.is_interactive()
    }

    /// The global logical point is inside the popup or a popup nested in it.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let point = self.frame.layout_point(x, y);
        self.areas.iter().any(|area| area.contains_point(point))
    }

    /// The global logical point is on the anchor (the popup's parent).
    pub fn anchor_contains(&self, x: f64, y: f64) -> bool {
        self.anchor.contains_point(self.frame.layout_point(x, y))
    }

    /// The pointer at the global logical point shows interest in the popup:
    /// it is on the anchor or, for an open interactive popup, on the popup.
    pub fn has_interest(&self, x: f64, y: f64) -> bool {
        self.anchor_contains(x, y) || (self.is_open_and_interactive() && self.contains(x, y))
    }
}

/// Every popup whose anchor is shown, open or not, in tree order.
pub fn popups(root: &ComputedDecorationNode) -> Vec<PopupInfo> {
    let mut found = Vec::new();
    collect_popups(root, None, &mut Vec::new(), &mut found);
    found
}

fn collect_popups(
    node: &ComputedDecorationNode,
    outer_layer: Option<PopupLayer>,
    enclosing: &mut Vec<String>,
    found: &mut Vec<PopupInfo>,
) {
    if node.style.visible == Some(false) {
        return;
    }
    for child in &node.children {
        let DecorationNodeKind::Popup(popup) = &child.kind else {
            collect_popups(child, outer_layer, enclosing, found);
            continue;
        };
        let index = found.len();
        found.push(PopupInfo {
            node_id: child.stable_id.clone(),
            // A nested popup is drawn in its outermost popup's pass.
            popup: PopupNode {
                layer: outer_layer.unwrap_or(popup.layer),
                ..*popup
            },
            handlers: child.interaction.popup.as_deref().cloned(),
            enclosing: enclosing.clone(),
            frame: child.frame,
            anchor: node.resolved_rect,
            areas: vec![child.resolved_bounds_rect()],
        });
        if !popup.open {
            continue;
        }
        let pushed = child
            .stable_id
            .clone()
            .map(|id| enclosing.push(id))
            .is_some();
        let nested_from = found.len();
        collect_popups(
            child,
            Some(outer_layer.unwrap_or(popup.layer)),
            enclosing,
            found,
        );
        if pushed {
            enclosing.pop();
        }
        let nested_areas = found[nested_from..]
            .iter()
            .filter(|nested| nested.is_open_and_interactive())
            .flat_map(|nested| nested.areas.first().copied())
            .collect::<Vec<_>>();
        found[index].areas.extend(nested_areas);
    }
}

/// The interactive popup under `point`, innermost first, with its
/// ancestors (nearest first).
pub(super) struct PopupHit<'a> {
    pub popup: &'a ComputedDecorationNode,
    pub ancestors: Vec<&'a ComputedDecorationNode>,
}

pub(super) fn popup_hit(root: &ComputedDecorationNode, point: LayoutPoint) -> Option<PopupHit<'_>> {
    let mut stack = Vec::new();
    find_popup_hit(root, point, &mut stack)
}

fn find_popup_hit<'a>(
    node: &'a ComputedDecorationNode,
    point: LayoutPoint,
    stack: &mut Vec<&'a ComputedDecorationNode>,
) -> Option<PopupHit<'a>> {
    if node.style.visible == Some(false) {
        return None;
    }
    // Popups nested deeper (and drawn above) are found first.
    stack.push(node);
    let nested = super::paint_ordered_children(node)
        .into_iter()
        .find_map(|child| find_popup_hit(child, point, stack));
    stack.pop();
    if nested.is_some() {
        return nested;
    }
    match &node.kind {
        DecorationNodeKind::Popup(popup)
            if popup.mode.is_interactive()
                && node.style.pointer_events_enabled()
                && node.resolved_bounds_rect().contains_point(point) =>
        {
            Some(PopupHit {
                popup: node,
                ancestors: stack.iter().rev().copied().collect(),
            })
        }
        _ => None,
    }
}

/// The nodes with interaction handlers under `point` inside a popup hit,
/// innermost first, followed by the popup's ancestors that have handlers:
/// inside a popup, hover covers the whole ancestor chain like DOM `:hover`.
pub(super) fn hover_chain(
    hit: &PopupHit<'_>,
    point: LayoutPoint,
) -> Vec<DecorationInteractionTarget> {
    let mut chain = Vec::new();
    collect_chain(hit.popup, point, &mut chain);
    for ancestor in &hit.ancestors {
        push_target(ancestor, &mut chain);
    }
    chain
}

fn collect_chain(
    node: &ComputedDecorationNode,
    point: LayoutPoint,
    chain: &mut Vec<DecorationInteractionTarget>,
) -> bool {
    if node.style.visible == Some(false) || !node.style.pointer_events_enabled() {
        return false;
    }
    if node
        .resolved_effective_clip
        .is_some_and(|clip| !clip.rect.contains_point(point))
        && !matches!(node.kind, DecorationNodeKind::Popup(_))
    {
        return false;
    }
    let inner = super::paint_ordered_children(node)
        .into_iter()
        .filter(|child| !matches!(child.kind, DecorationNodeKind::Popup(_)))
        .any(|child| collect_chain(child, point, chain));
    if inner || node.resolved_rect.contains_point(point) {
        push_target(node, chain);
        return true;
    }
    false
}

fn push_target(node: &ComputedDecorationNode, chain: &mut Vec<DecorationInteractionTarget>) {
    if node.interaction.has_any()
        && let Some(node_id) = node.stable_id.clone()
    {
        chain.push(DecorationInteractionTarget {
            node_id,
            handlers: node.interaction.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(x: i32, y: i32, w: i32, h: i32) -> ResolvedLogicalRect {
        ResolvedLogicalRect::from_px(x, y, w, h)
    }

    fn size(w: i32, h: i32) -> (ResolvedLayoutValue, ResolvedLayoutValue) {
        (
            ResolvedLayoutValue::from_raw(w),
            ResolvedLayoutValue::from_raw(h),
        )
    }

    fn popup(placement: PopupPlacement, align: PopupAlign) -> PopupNode {
        PopupNode {
            placement,
            align,
            ..PopupNode::default()
        }
    }

    #[test]
    fn places_on_each_side_with_alignment() {
        let anchor = px(100, 100, 40, 20);
        let offset = ResolvedLayoutValue::from_raw(4);
        let at =
            |placement, align| place(&popup(placement, align), anchor, size(60, 10), offset, None);
        assert_eq!(
            at(PopupPlacement::Bottom, PopupAlign::Center),
            px(90, 124, 60, 10)
        );
        assert_eq!(
            at(PopupPlacement::Top, PopupAlign::Start),
            px(100, 86, 60, 10)
        );
        assert_eq!(
            at(PopupPlacement::Right, PopupAlign::End),
            px(144, 110, 60, 10)
        );
        assert_eq!(
            at(PopupPlacement::Left, PopupAlign::Center),
            px(36, 105, 60, 10)
        );
    }

    #[test]
    fn flips_to_the_side_with_more_room_and_slides_onto_the_output() {
        let viewport = Some(px(0, 0, 200, 200));
        let offset = ResolvedLayoutValue::from_raw(4);
        // A button at the top edge: no room above, so the popup goes below.
        let top_button = px(170, 2, 20, 20);
        let placed = place(
            &popup(PopupPlacement::Top, PopupAlign::Center),
            top_button,
            size(60, 10),
            offset,
            viewport,
        );
        assert_eq!(placed, px(140, 26, 60, 10));
        // Not enough room on either side: stay on the side with more room.
        let tall = place(
            &popup(PopupPlacement::Bottom, PopupAlign::Start),
            px(0, 150, 20, 20),
            size(20, 180),
            offset,
            viewport,
        );
        assert_eq!(tall.y.raw(), 150 - 4 - 180);
    }

    #[test]
    fn scopes_match_buffer_keys_of_their_subtree_only() {
        let scopes = PopupScopes {
            scopes: vec![("root/child-1".into(), PopupLayer::Top)],
        };
        assert_eq!(scopes.layer_of("root/child-1:box"), Some(PopupLayer::Top));
        assert_eq!(
            scopes.layer_of("root/child-1/child-0:label"),
            Some(PopupLayer::Top)
        );
        assert_eq!(scopes.layer_of("root/child-10:box"), None);
        assert_eq!(scopes.layer_of("root:box"), None);
        assert!(scopes.includes(DecorationPart::Window, "root/child-10:box"));
        assert!(!scopes.includes(DecorationPart::Window, "root/child-1:box"));
        assert!(scopes.includes(DecorationPart::Popups(PopupLayer::Top), "root/child-1:box"));
        assert!(!scopes.includes(
            DecorationPart::Popups(PopupLayer::Window),
            "root/child-1:box"
        ));
    }
}
