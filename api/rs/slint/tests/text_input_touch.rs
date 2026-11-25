// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

mod common;

use i_slint_core::input::TouchPhase;
use i_slint_core::lengths::LogicalPoint;
use i_slint_core::platform::InternalEvent;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{LogicalPosition, SharedString};

slint::slint! {
    export component TouchComponent inherits Window {
        in-out property <string> text <=> input.text;
        out property <int> cursor: input.cursor-position-byte-offset;
        out property <int> anchor: input.anchor-position-byte-offset;
        input := TextInput {
            x: 10px;
            y: 10px;
            width: 380px;
            height: 40px;
            font-size: 20px;
            single-line: true;
        }
    }
}

fn touch(ui: &TouchComponent, phase: TouchPhase, x: f32) {
    let position = LogicalPoint::new(x, 30.0);
    ui.window().dispatch_event(WindowEvent::internal(InternalEvent::Touch {
        id: 0,
        position,
        phase,
    }));
    slint::platform::update_timers_and_animations();
}

/// Where a tap at `x` puts the cursor.
fn tapped(ui: &TouchComponent, x: f32) -> i32 {
    touch(ui, TouchPhase::Started, x);
    touch(ui, TouchPhase::Ended, x);
    ui.get_cursor()
}

#[test]
fn a_finger_drag_moves_the_cursor_and_never_selects_or_moves_text() {
    let _window = common::setup(400, 60);
    let ui = TouchComponent::new().unwrap();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::WindowActiveChanged(true));

    for text in ["Misurata Medical Centre", "مركز مصراتة الطبي", "جهاز BC-5150 في المختبر"]
    {
        ui.set_text(SharedString::from(text));
        // Inside the words in each direction: past a right-to-left line's end is its start.
        let start = tapped(&ui, 20.0);
        let end = tapped(&ui, 120.0);
        assert_ne!(start, end, "{text}: a tap places the cursor at the touched character");

        // A drag across the words, as a finger scrolls or aims, from one place to another.
        touch(&ui, TouchPhase::Started, 20.0);
        for x in [50.0, 80.0, 100.0, 120.0] {
            touch(&ui, TouchPhase::Moved, x);
            assert_eq!(
                ui.get_cursor(),
                ui.get_anchor(),
                "{text}: a finger drag selects nothing at {x}"
            );
        }
        touch(&ui, TouchPhase::Ended, 120.0);
        assert_eq!(ui.get_text(), text, "{text}: a finger drag never moves the text");
        assert_eq!(
            (ui.get_cursor(), ui.get_anchor()),
            (end, end),
            "{text}: the cursor where a tap there puts it"
        );
    }
}

#[test]
fn a_mouse_drag_still_selects() {
    let _window = common::setup(400, 60);
    let ui = TouchComponent::new().unwrap();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::WindowActiveChanged(true));
    ui.set_text("Misurata Medical Centre".into());
    let button = PointerEventButton::Left;
    let at = |x: f32| LogicalPosition::new(x, 30.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed { position: at(20.0), button });
    ui.window().dispatch_event(WindowEvent::PointerMoved { position: at(200.0) });
    ui.window().dispatch_event(WindowEvent::PointerReleased { position: at(200.0), button });
    assert_ne!(ui.get_cursor(), ui.get_anchor(), "a mouse drag selects");
}
