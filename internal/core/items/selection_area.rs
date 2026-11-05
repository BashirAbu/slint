// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! The `SelectionArea` element: one selection across the `StyledText` elements under it, as on a
//! page in a browser.
//!
//! Each text holds and draws its own selected range. The rest only matters while a pointer selects,
//! and one pointer selects at a time, so it is thread-local rather than on the item, where the C++
//! bindings would have to carry it.

use super::text::{
    next_paragraph_boundary, next_word_boundary, prev_paragraph_boundary, prev_word_boundary,
};
use super::{
    BuiltInMouseCursor, Flickable, Item, ItemConsts, ItemRc, ItemRendererRef, ItemTreeVTable,
    MouseCursorInner, PointArg, PointerEventButton, RenderingResult, StyledTextItem,
};
use crate::input::{
    FocusEvent, FocusEventResult, FocusReason, InputEventFilterResult, InputEventResult,
    InternalKeyEvent, KeyEventResult, KeyEventType, MouseEvent, StandardShortcut, key_codes,
};
use crate::item_rendering::CachedRenderingData;
use crate::item_tree::{ItemWeak, ParentItemTraversalMode};
use crate::layout::{LayoutInfo, Orientation};
use crate::lengths::{LogicalLength, LogicalPoint, LogicalRect, LogicalSize};
#[cfg(feature = "rtti")]
use crate::rtti::*;
use crate::timers::{Timer, TimerMode};
use crate::window::{WindowAdapter, WindowInner};
use crate::{Callback, Coord, Property};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use const_field_offset::FieldOffsets;
use core::cell::RefCell;
use core::ops::{ControlFlow, Range};
use core::pin::Pin;
use core::time::Duration;
use i_slint_core_macros::*;
use vtable::VRcMapped;

/// How far a press on a link travels before it selects instead of clicking.
const DRAG_THRESHOLD: Coord = 3 as Coord;
/// How far a resting finger may wander and still be a long press.
const LONG_PRESS_SLOP: Coord = 8 as Coord;
/// How long a finger rests to select a word. A Flickable holds a press back by its forward delay
/// first, and within half a second of the press still takes a move, so this ends just after that:
/// from then on, moves reach the area.
const LONG_PRESS: Duration = Duration::from_millis(450);
const AUTO_SCROLL_TICK: Duration = Duration::from_millis(16);
/// The most an auto-scroll tick moves, however far past the edge the pointer is.
const AUTO_SCROLL_MAX: Coord = 30 as Coord;

/// The implementation of the `SelectionArea` element
#[repr(C)]
#[derive(FieldOffsets, Default, SlintElement)]
#[pin]
pub struct SelectionArea {
    pub enabled: Property<bool>,
    pub has_selection: Property<bool>,
    pub has_focus: Property<bool>,
    pub menu_requested: Callback<PointArg>,
    pub cached_rendering_data: CachedRenderingData,
}

/// What a press selects: a caret to drag from, the word under it, or its paragraph.
#[derive(Clone, Copy, Default)]
enum Unit {
    #[default]
    Character,
    Word,
    Paragraph,
}

impl Unit {
    fn of_click(click_count: u8) -> Self {
        match click_count % 3 {
            0 => Self::Character,
            1 => Self::Word,
            _ => Self::Paragraph,
        }
    }

    fn around(self, text: &str, offset: usize) -> Range<usize> {
        match self {
            Self::Character => offset..offset,
            Self::Word => prev_word_boundary(text, offset)..next_word_boundary(text, offset),
            Self::Paragraph => {
                prev_paragraph_boundary(text, offset)..next_paragraph_boundary(text, offset)
            }
        }
    }
}

/// What a held pointer is doing.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    /// The right button: its release asks for the menu.
    Menu,
    /// The left button on a link: a drag selects, and otherwise the click is the link's.
    Link,
    /// A finger at rest: the long press selects, and a lift before it is a tap.
    Finger,
    /// A finger on a text in a Flickable of its own that scrolls: until the long press, the finger
    /// is that Flickable's to swipe.
    Held,
    /// Moving the pointer moves the end of the selection.
    Selecting,
}

/// A pointer held down over an area. Positions are in the window's coordinates: the area moves
/// under a pointer when it scrolls.
struct Pressed {
    area: ItemWeak,
    stage: Stage,
    from: LogicalPoint,
    now: LogicalPoint,
    unit: Unit,
}

#[derive(Default)]
struct Selecting {
    /// The text a selection grows from and the range of the press in it.
    anchor: Option<(ItemWeak, Range<usize>)>,
    pressed: Option<Pressed>,
    /// As the area's filter saw it: the event `input_event` gets counts only clicks on the top item.
    click_count: u8,
    long_press: Option<Timer>,
    auto_scroll: Option<Timer>,
}

crate::thread_local! {
    static SELECTING: RefCell<Selecting> = RefCell::default();
}

fn with_state<R>(f: impl FnOnce(&mut Selecting) -> R) -> R {
    SELECTING.with(|state| f(&mut state.borrow_mut()))
}

/// The stage of the pointer held over `area`, if one is.
fn stage_of(area: &ItemRc) -> Option<Stage> {
    let area = area.downgrade();
    with_state(|s| s.pressed.as_ref().filter(|p| p.area == area).map(|p| p.stage))
}

/// The pointer is gone: nothing goes on selecting or scrolling. A timer is dropped outside the
/// borrow, since it may be the one calling.
fn release_pointer() {
    let _timers = with_state(|s| {
        s.pressed = None;
        (s.long_press.take(), s.auto_scroll.take())
    });
}

/// The top left of `item`, in the window's coordinates.
fn origin_of(item: &ItemRc) -> LogicalPoint {
    item.map_to_window(item.geometry().origin)
}

fn styled(item: &ItemRc) -> Option<VRcMapped<ItemTreeVTable, StyledTextItem>> {
    item.downcast::<StyledTextItem>()
}

/// The texts that can be selected, in reading order: scrolled out of view counts, hidden does not.
fn texts_under(area: &ItemRc) -> Vec<ItemRc> {
    let mut texts = every_text_under(area);
    texts.retain(|text| {
        styled(text).is_some_and(|text| {
            text.as_pin_ref().text().paragraphs.iter().any(|paragraph| !paragraph.text.is_empty())
        })
    });
    texts
}

/// The texts under the area in reading order, those with nothing in them too: a copy keeps the
/// place of an empty one that has a separator, such as a table's empty cell.
fn every_text_under(area: &ItemRc) -> Vec<ItemRc> {
    let mut texts = Vec::new();
    area.visit_descendants::<()>(|item| {
        if styled(item).is_some() && item.is_visible_or_clipped_by_flickable() {
            texts.push(item.clone());
        }
        ControlFlow::Continue(())
    });
    texts
}

/// Whether a Flickable between the area and `text` can scroll, as a wide table's does.
fn pans_under(area: &ItemRc, text: &ItemRc) -> bool {
    let mut parent = text.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(item) = parent.filter(|item| item != area) {
        if let Some(flickable) = item.downcast::<Flickable>()
            && super::flickable::FlickableData::can_pan(flickable.as_pin_ref(), &item)
        {
            return true;
        }
        parent = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    false
}

/// The text exactly under `position`, in the area's coordinates, and the position in that text's.
fn text_at(area: &ItemRc, position: LogicalPoint) -> Option<(ItemRc, LogicalPoint)> {
    let window = position + origin_of(area).to_vector();
    area.visit_descendants(|item| {
        if styled(item).is_some() {
            let inside = window - origin_of(item).to_vector();
            if LogicalRect::from_size(item.geometry().size).contains(inside)
                && item.is_visible_or_clipped_by_flickable()
            {
                return ControlFlow::Break((item.clone(), inside));
            }
        }
        ControlFlow::Continue(())
    })
}

fn raw_text(text: &ItemRc) -> String {
    styled(text).map_or_else(String::new, |text| {
        crate::styled_text::get_raw_text(&text.as_pin_ref().text()).into_owned()
    })
}

/// The byte offset in `text`'s raw text nearest `position`, in the text's own coordinates.
#[cfg_attr(not(feature = "shared-parley"), allow(unused))]
fn offset_at(
    window_adapter: &Rc<dyn WindowAdapter>,
    text: &ItemRc,
    position: LogicalPoint,
) -> usize {
    #[cfg(feature = "shared-parley")]
    if let Some(item) = styled(text) {
        let scale_factor = crate::lengths::ScaleFactor::new(
            WindowInner::from_pub(window_adapter.window()).scale_factor(),
        );
        return crate::textlayout::sharedparley::byte_offset_under_cursor(
            scale_factor,
            item.as_pin_ref(),
            text,
            text.geometry().size,
            position * scale_factor,
            window_adapter.window(),
            window_adapter.renderer().text_layout_cache(),
        )
        .unwrap_or(0);
    }
    0
}

#[cfg_attr(not(feature = "shared-parley"), allow(unused))]
fn is_link_at(
    window_adapter: &Rc<dyn WindowAdapter>,
    text: &ItemRc,
    position: LogicalPoint,
) -> bool {
    #[cfg(feature = "shared-parley")]
    if let Some(item) = styled(text) {
        let scale_factor = crate::lengths::ScaleFactor::new(
            WindowInner::from_pub(window_adapter.window()).scale_factor(),
        );
        return crate::textlayout::sharedparley::link_under_cursor(
            scale_factor,
            item.as_pin_ref(),
            text,
            text.geometry().size,
            position * scale_factor,
            window_adapter.window(),
            window_adapter.renderer().text_layout_cache(),
        )
        .is_some();
    }
    false
}

/// The position in `texts` a pointer at `window` stands for: the text it is over, else the nearest
/// one level with it, as in a table's row, else the start of the next one below it, else the end of
/// the last.
fn nearest_position(
    window_adapter: &Rc<dyn WindowAdapter>,
    texts: &[ItemRc],
    window: LogicalPoint,
) -> Option<(usize, usize)> {
    let rect_of = |text: &ItemRc| LogicalRect::new(origin_of(text), text.geometry().size);
    let over = texts.iter().position(|text| rect_of(text).contains(window));
    let beside = || {
        let away = |rect: LogicalRect| (rect.min_x() - window.x).max(window.x - rect.max_x());
        let level = texts.iter().enumerate().map(|(index, text)| (index, rect_of(text)));
        let level = level.filter(|(_, rect)| (rect.min_y()..rect.max_y()).contains(&window.y));
        level.min_by(|(_, a), (_, b)| away(*a).total_cmp(&away(*b))).map(|(index, _)| index)
    };
    let below = || texts.iter().position(|text| window.y < rect_of(text).max_y());
    let Some(index) = over.or_else(beside).or_else(below) else {
        let last = texts.len().checked_sub(1)?;
        return Some((last, raw_text(&texts[last]).len()));
    };
    let text = &texts[index];
    let rect = rect_of(text);
    if window.y < rect.min_y() {
        return Some((index, 0));
    }
    Some((index, offset_at(window_adapter, text, window - rect.origin.to_vector())))
}

/// The Flickable the area scrolls in.
fn scrolled_by(area: &ItemRc) -> Option<ItemRc> {
    let mut parent = area.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(item) = parent {
        if item.downcast::<Flickable>().is_some() {
            return Some(item);
        }
        parent = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    None
}

impl Item for SelectionArea {
    fn init(self: Pin<&Self>, _self_rc: &ItemRc) {}

    fn deinit(self: Pin<&Self>, _window_adapter: &Rc<dyn WindowAdapter>) {}

    fn layout_info(
        self: Pin<&Self>,
        _orientation: Orientation,
        _cross_axis_constraint: Coord,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
    ) -> LayoutInfo {
        LayoutInfo { stretch: 1., ..LayoutInfo::default() }
    }

    fn input_event_filter_before_children(
        self: Pin<&Self>,
        event: &MouseEvent,
        window_adapter: &Rc<dyn WindowAdapter>,
        self_rc: &ItemRc,
        cursor: &mut MouseCursorInner,
    ) -> InputEventFilterResult {
        if !self.enabled() {
            return InputEventFilterResult::ForwardAndIgnore;
        }
        let window_of = |position: &LogicalPoint| *position + origin_of(self_rc).to_vector();
        match event {
            MouseEvent::Pressed { position, button, click_count, .. } => {
                release_pointer();
                with_state(|s| s.click_count = *click_count);
                if !matches!(button, PointerEventButton::Left | PointerEventButton::Right) {
                    return InputEventFilterResult::ForwardEvent;
                }
                let Some((text, inside)) = text_at(self_rc, *position) else {
                    // A gap, or a control under the area: the control keeps its press, and only
                    // what nothing takes comes back to `input_event`.
                    return InputEventFilterResult::ForwardEvent;
                };
                let finger = event.is_from_touch() && *button == PointerEventButton::Left;
                if !is_link_at(window_adapter, &text, inside)
                    && finger
                    && pans_under(self_rc, &text)
                {
                    let window = window_of(position);
                    with_state(|s| {
                        s.pressed = Some(Pressed {
                            area: self_rc.downgrade(),
                            stage: Stage::Held,
                            from: window,
                            now: window,
                            unit: Unit::Word,
                        })
                    });
                    self.wait_for_long_press(window_adapter, self_rc);
                    return InputEventFilterResult::ForwardAndInterceptGrab;
                }
                if !is_link_at(window_adapter, &text, inside) {
                    return InputEventFilterResult::Intercept;
                }
                if *button == PointerEventButton::Left && !event.is_from_touch() {
                    let window = window_of(position);
                    with_state(|s| {
                        s.pressed = Some(Pressed {
                            area: self_rc.downgrade(),
                            stage: Stage::Link,
                            from: window,
                            now: window,
                            unit: Unit::of_click(*click_count),
                        })
                    });
                }
                InputEventFilterResult::ForwardEvent
            }
            // The finger swipes the Flickable, unless it rested and now selects.
            MouseEvent::Moved { position, .. } if stage_of(self_rc) == Some(Stage::Held) => {
                let window = window_of(position);
                let moved = with_state(|s| {
                    s.pressed.as_mut().map_or(0 as Coord, |pressed| {
                        pressed.now = window;
                        (window - pressed.from).length()
                    })
                });
                if moved > LONG_PRESS_SLOP {
                    let _timer = with_state(|s| s.long_press.take());
                }
                InputEventFilterResult::ForwardAndInterceptGrab
            }
            MouseEvent::Moved { .. } | MouseEvent::Released { .. }
                if event.is_from_touch() && stage_of(self_rc) == Some(Stage::Selecting) =>
            {
                InputEventFilterResult::Intercept
            }
            MouseEvent::Moved { position, .. } => {
                if stage_of(self_rc) == Some(Stage::Link) {
                    let window = window_of(position);
                    let moved = with_state(|s| {
                        s.pressed.as_ref().map_or(0 as Coord, |p| (window - p.from).length())
                    });
                    if moved > DRAG_THRESHOLD {
                        return InputEventFilterResult::Intercept;
                    }
                }
                let Some((text, inside)) = text_at(self_rc, *position) else {
                    return InputEventFilterResult::ForwardAndIgnore;
                };
                *cursor = MouseCursorInner::BuiltIn(BuiltInMouseCursor::Text);
                // Hovering stays with the area, unless a link wants it: clicks only count up while
                // the top item stays the same, and the release of a press on the area hovers.
                if is_link_at(window_adapter, &text, inside) {
                    InputEventFilterResult::ForwardEvent
                } else {
                    InputEventFilterResult::Intercept
                }
            }
            MouseEvent::Released { .. } => {
                let stage = stage_of(self_rc);
                // A tap clears the selection, as on any text; a swipe leaves it.
                let tapped = stage == Some(Stage::Held) && with_state(|s| s.long_press.is_some());
                if matches!(stage, Some(Stage::Link | Stage::Held)) {
                    release_pointer();
                }
                if tapped {
                    self.clear_selection(window_adapter, self_rc);
                }
                InputEventFilterResult::ForwardEvent
            }
            _ => InputEventFilterResult::ForwardEvent,
        }
    }

    fn input_event(
        self: Pin<&Self>,
        event: &MouseEvent,
        window_adapter: &Rc<dyn WindowAdapter>,
        self_rc: &ItemRc,
        _cursor: &mut MouseCursorInner,
    ) -> InputEventResult {
        if !self.enabled() {
            return InputEventResult::EventIgnored;
        }
        let window_of = |position: &LogicalPoint| *position + origin_of(self_rc).to_vector();
        match event {
            MouseEvent::Pressed { position, button, .. } => {
                let unit = Unit::of_click(with_state(|s| s.click_count));
                let (stage, unit) = match button {
                    PointerEventButton::Right => (Stage::Menu, unit),
                    PointerEventButton::Left if event.is_from_touch() => {
                        (Stage::Finger, Unit::Word)
                    }
                    PointerEventButton::Left => (Stage::Selecting, unit),
                    _ => return InputEventResult::EventIgnored,
                };
                let window = window_of(position);
                let area = self_rc.downgrade();
                with_state(|s| {
                    s.pressed = Some(Pressed { area, stage, from: window, now: window, unit })
                });
                match stage {
                    Stage::Menu => self.take_focus(window_adapter, self_rc),
                    Stage::Selecting => self.begin(window_adapter, self_rc),
                    Stage::Finger if text_at(self_rc, *position).is_some() => {
                        self.wait_for_long_press(window_adapter, self_rc)
                    }
                    _ => {}
                }
                InputEventResult::GrabMouse
            }
            MouseEvent::Moved { position, .. } => {
                let Some(stage) = stage_of(self_rc) else {
                    return InputEventResult::EventAccepted;
                };
                // The finger is the Flickable's, which may not have it yet.
                if stage == Stage::Held {
                    return InputEventResult::EventIgnored;
                }
                let window = window_of(position);
                let moved = with_state(|s| {
                    s.pressed.as_mut().map_or(0 as Coord, |pressed| {
                        pressed.now = window;
                        (window - pressed.from).length()
                    })
                });
                match stage {
                    Stage::Menu | Stage::Held => {}
                    Stage::Finger => {
                        if moved > LONG_PRESS_SLOP {
                            let _timer = with_state(|s| s.long_press.take());
                        }
                    }
                    Stage::Link | Stage::Selecting => {
                        if stage == Stage::Link {
                            self.begin(window_adapter, self_rc);
                        }
                        self.extend(window_adapter, self_rc);
                        self.keep_scrolling(window_adapter, self_rc);
                    }
                }
                InputEventResult::GrabMouse
            }
            MouseEvent::Released { position, .. } => {
                let Some(stage) = stage_of(self_rc) else {
                    return InputEventResult::EventIgnored;
                };
                release_pointer();
                if stage == Stage::Menu || (stage == Stage::Selecting && event.is_from_touch()) {
                    let position = crate::lengths::logical_position_to_api(*position);
                    Self::FIELD_OFFSETS.menu_requested().apply_pin(self).call(&(position,));
                }
                if stage == Stage::Finger {
                    self.clear_selection(window_adapter, self_rc);
                }
                InputEventResult::EventAccepted
            }
            MouseEvent::Exit => {
                if stage_of(self_rc).is_some() {
                    release_pointer();
                }
                InputEventResult::EventIgnored
            }
            _ => InputEventResult::EventIgnored,
        }
    }

    fn capture_key_event(
        self: Pin<&Self>,
        _: &InternalKeyEvent,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
    ) -> KeyEventResult {
        KeyEventResult::EventIgnored
    }

    fn key_event(
        self: Pin<&Self>,
        event: &InternalKeyEvent,
        window_adapter: &Rc<dyn WindowAdapter>,
        self_rc: &ItemRc,
    ) -> KeyEventResult {
        if !self.enabled() || event.event_type != KeyEventType::KeyPressed {
            return KeyEventResult::EventIgnored;
        }
        let key = event.key_event.text.as_str();
        // `shortcut` leaves copying to the browser's own event on the web, which goes to the hidden
        // field of a focused `TextInput` and so never reaches an area.
        let copying = matches!(event.shortcut(), Some(StandardShortcut::Copy))
            || (cfg!(target_arch = "wasm32") && event.key_event.modifiers.control && key == "c");
        if copying {
            self.copy(window_adapter, self_rc);
        } else if matches!(event.shortcut(), Some(StandardShortcut::SelectAll)) {
            self.select_all(window_adapter, self_rc);
        } else if self.has_selection() && key.starts_with(key_codes::Escape) {
            self.clear_selection(window_adapter, self_rc);
        } else {
            return KeyEventResult::EventIgnored;
        }
        KeyEventResult::EventAccepted
    }

    fn focus_event(
        self: Pin<&Self>,
        event: &FocusEvent,
        window_adapter: &Rc<dyn WindowAdapter>,
        self_rc: &ItemRc,
    ) -> FocusEventResult {
        match event {
            // Reached by the pointer, not by Tab: there is nothing to do at it by keyboard alone.
            FocusEvent::FocusIn(FocusReason::TabNavigation) => FocusEventResult::FocusIgnored,
            FocusEvent::FocusIn(_) if !self.enabled() => FocusEventResult::FocusIgnored,
            FocusEvent::FocusIn(_) => {
                Self::FIELD_OFFSETS.has_focus().apply_pin(self).set(true);
                FocusEventResult::FocusAccepted
            }
            FocusEvent::FocusOut(reason) => {
                Self::FIELD_OFFSETS.has_focus().apply_pin(self).set(false);
                if !matches!(reason, FocusReason::WindowActivation | FocusReason::PopupActivation) {
                    self.clear_selection(window_adapter, self_rc);
                }
                FocusEventResult::FocusAccepted
            }
        }
    }

    fn render(
        self: Pin<&Self>,
        _backend: &mut ItemRendererRef,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) -> RenderingResult {
        RenderingResult::ContinueRenderingChildren
    }

    fn bounding_rect(
        self: Pin<&Self>,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
        geometry: LogicalRect,
    ) -> LogicalRect {
        geometry
    }

    fn clips_children(self: Pin<&Self>) -> bool {
        false
    }
}

impl ItemConsts for SelectionArea {
    const cached_rendering_data_offset: const_field_offset::FieldOffset<Self, CachedRenderingData> =
        Self::FIELD_OFFSETS.cached_rendering_data().as_unpinned_projection();
}

impl SelectionArea {
    /// Copies the selected text to the clipboard. Before each text after the first stands its
    /// `selection-separator`, or a blank line when it has none; those of the texts with nothing
    /// selected count once something after them is copied, so an empty cell keeps its column. With
    /// nothing selected, the clipboard stays as it was.
    pub fn copy(self: Pin<&Self>, window_adapter: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        let (mut copied, mut between) = (String::new(), String::new());
        for text in every_text_under(self_rc) {
            let Some(item) = styled(&text) else { continue };
            let item = item.as_pin_ref();
            let (raw, separator) = (raw_text(&text), item.selection_separator());
            if raw.is_empty() && separator.is_empty() {
                continue;
            }
            if !copied.is_empty() {
                match separator.is_empty() {
                    true => between = String::from("\n\n"),
                    false => between.push_str(&separator),
                }
            }
            let range = item.selected_range();
            if !range.is_empty() {
                copied.push_str(&core::mem::take(&mut between));
                copied.push_str(&raw[range]);
            }
        }
        if !copied.is_empty() {
            WindowInner::from_pub(window_adapter.window())
                .context()
                .platform()
                .set_clipboard_text(&copied, crate::platform::Clipboard::DefaultClipboard);
        }
    }

    /// Selects every text under the area.
    pub fn select_all(self: Pin<&Self>, _: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        let texts = texts_under(self_rc);
        let (Some(first), Some(last)) = (texts.first(), texts.last()) else { return };
        with_state(|s| s.anchor = Some((first.downgrade(), 0..0)));
        self.show(&texts, (0, 0), (texts.len() - 1, raw_text(last).len()));
    }

    /// Selects nothing.
    pub fn clear_selection(self: Pin<&Self>, _: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        with_state(|s| s.anchor = None);
        self.show(&texts_under(self_rc), (0, 0), (0, 0));
    }

    fn take_focus(self: Pin<&Self>, window_adapter: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        WindowInner::from_pub(window_adapter.window()).set_focus_item(
            self_rc,
            true,
            FocusReason::PointerClick,
        );
    }

    /// The selection starts where the pointer went down: at the press, unless Shift keeps its old
    /// start.
    fn begin(self: Pin<&Self>, window_adapter: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        self.take_focus(window_adapter, self_rc);
        let texts = texts_under(self_rc);
        let shift = window_adapter.window().0.context().0.modifiers.get().shift();
        let keeps_start = shift
            && with_state(|s| {
                s.anchor.as_ref().is_some_and(|(text, _)| {
                    texts.iter().any(|candidate| candidate.downgrade() == *text)
                })
            });
        let Some((from, unit)) = with_state(|s| {
            let pressed = s.pressed.as_mut()?;
            pressed.stage = Stage::Selecting;
            if keeps_start {
                pressed.unit = Unit::Character;
            }
            Some((pressed.from, pressed.unit))
        }) else {
            return;
        };
        if !keeps_start
            && let Some((index, offset)) = nearest_position(window_adapter, &texts, from)
        {
            let range = unit.around(&raw_text(&texts[index]), offset);
            with_state(|s| s.anchor = Some((texts[index].downgrade(), range)));
        }
        self.select_to(window_adapter, &texts, from);
    }

    /// The end of the selection follows the pointer.
    fn extend(self: Pin<&Self>, window_adapter: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        let Some(now) = with_state(|s| s.pressed.as_ref().map(|p| p.now)) else { return };
        self.select_to(window_adapter, &texts_under(self_rc), now);
    }

    /// Selects from the anchor to the pointer at `window`.
    fn select_to(
        self: Pin<&Self>,
        window_adapter: &Rc<dyn WindowAdapter>,
        texts: &[ItemRc],
        window: LogicalPoint,
    ) {
        let Some((index, offset)) = nearest_position(window_adapter, texts, window) else { return };
        let unit = with_state(|s| s.pressed.as_ref().map_or(Unit::Character, |p| p.unit));
        let around = unit.around(&raw_text(&texts[index]), offset);
        let anchor = with_state(|s| s.anchor.clone()).and_then(|(text, range)| {
            let index = texts.iter().position(|candidate| candidate.downgrade() == text)?;
            Some((index, range))
        });
        let (anchor_index, anchor) = anchor.unwrap_or_else(|| (index, around.clone()));
        if (index, around.start) < (anchor_index, anchor.start) {
            self.show(texts, (index, around.start), (anchor_index, anchor.end));
        } else {
            self.show(texts, (anchor_index, anchor.start), (index, around.end));
        }
    }

    /// Selects from `start` to `end`, each a text's index and a byte offset in it, on those texts
    /// and the ones between them, and nothing on the others.
    fn show(self: Pin<&Self>, texts: &[ItemRc], start: (usize, usize), end: (usize, usize)) {
        for (index, text) in texts.iter().enumerate() {
            let Some(item) = styled(text) else { continue };
            let range = if (start.0..=end.0).contains(&index) {
                let from = if index == start.0 { start.1 } else { 0 };
                let to = if index == end.0 { end.1 } else { raw_text(text).len() };
                from..to
            } else {
                0..0
            };
            item.as_pin_ref().set_selected_range(range);
        }
        Self::FIELD_OFFSETS.has_selection().apply_pin(self).set(start != end);
    }

    /// A finger that rests selects the word under it.
    fn wait_for_long_press(
        self: Pin<&Self>,
        window_adapter: &Rc<dyn WindowAdapter>,
        self_rc: &ItemRc,
    ) {
        let area = self_rc.downgrade();
        let timer = WindowInner::from_pub(window_adapter.window()).context().new_timer();
        timer.start(TimerMode::SingleShot, LONG_PRESS, move || {
            let Some(area) = area.upgrade() else { return };
            let (Some(item), Some(window_adapter)) =
                (area.downcast::<Self>(), area.window_adapter())
            else {
                return;
            };
            item.as_pin_ref().begin(&window_adapter, &area);
        });
        with_state(|s| s.long_press = Some(timer));
    }

    /// While the pointer is past the edge of the Flickable the area scrolls in, the Flickable
    /// scrolls and the selection goes along.
    fn keep_scrolling(self: Pin<&Self>, window_adapter: &Rc<dyn WindowAdapter>, self_rc: &ItemRc) {
        if with_state(|s| s.auto_scroll.is_some()) || !Self::scroll_step(self_rc) {
            return;
        }
        self.extend(window_adapter, self_rc);
        let area = self_rc.downgrade();
        let timer = WindowInner::from_pub(window_adapter.window()).context().new_timer();
        timer.start(TimerMode::Repeated, AUTO_SCROLL_TICK, move || {
            let Some(area) = area.upgrade() else { return };
            let (Some(item), Some(window_adapter)) =
                (area.downcast::<Self>(), area.window_adapter())
            else {
                return;
            };
            if Self::scroll_step(&area) {
                item.as_pin_ref().extend(&window_adapter, &area);
            } else {
                let _timer = with_state(|s| s.auto_scroll.take());
            }
        });
        with_state(|s| s.auto_scroll = Some(timer));
    }

    /// Scrolls one tick toward the pointer. False when the pointer is inside, or the end is reached.
    fn scroll_step(self_rc: &ItemRc) -> bool {
        let Some(flickable_rc) = scrolled_by(self_rc) else { return false };
        let Some(flickable) = flickable_rc.downcast::<Flickable>() else { return false };
        let Some(now) = with_state(|s| s.pressed.as_ref().map(|p| p.now)) else { return false };
        let flickable = flickable.as_pin_ref();
        let height = flickable_rc.geometry().height();
        let y = now.y - origin_of(&flickable_rc).y;
        let past = if y < 0 as Coord {
            y
        } else if y > height {
            y - height
        } else {
            return false;
        };
        let lowest = (height - flickable.content_height().0).min(0 as Coord);
        let step = (past / 2 as Coord).clamp(-AUTO_SCROLL_MAX, AUTO_SCROLL_MAX);
        let before = flickable.content_y().0;
        let after = (before - step).clamp(lowest, 0 as Coord);
        Flickable::FIELD_OFFSETS.content_y().apply_pin(flickable).set(LogicalLength::new(after));
        after != before
    }
}

#[cfg(feature = "ffi")]
mod ffi {
    use super::*;

    macro_rules! member_function {
        ($name:ident, $method:ident) => {
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $name(
                s: Pin<&SelectionArea>,
                window_adapter: *const crate::window::ffi::WindowAdapterRcOpaque,
                self_component: &vtable::VRc<crate::item_tree::ItemTreeVTable>,
                self_index: u32,
            ) {
                unsafe {
                    let window_adapter = &*(window_adapter as *const Rc<dyn WindowAdapter>);
                    let self_rc = ItemRc::new(self_component.clone(), self_index);
                    s.$method(window_adapter, &self_rc);
                }
            }
        };
    }
    member_function!(slint_selectionarea_copy, copy);
    member_function!(slint_selectionarea_select_all, select_all);
    member_function!(slint_selectionarea_clear_selection, clear_selection);
}
