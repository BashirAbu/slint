// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

mod common;

use i_slint_core::input::{InternalKeyEvent, KeyEventType};

#[test]
fn assigning_text_ends_an_open_composition() {
    let _window = common::setup(200, 100);

    slint::slint! {
        export component ComposingComponent inherits Window {
            forward-focus: input;
            in-out property <string> text <=> input.text;
            out property <string> composing: input.preedit-text;
            input := TextInput { }
        }
    }

    let ui = ComposingComponent::new().unwrap();
    ui.show().unwrap();
    ui.window().dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));

    let mut event = InternalKeyEvent::default();
    event.event_type = KeyEventType::UpdateComposition;
    event.preedit_text = "mes".into();
    event.cursor_position = Some(3);
    ui.window().dispatch_event(slint::platform::WindowEvent::internal(event));
    assert_eq!(ui.get_composing(), "mes");

    // An application picking a suggestion replaces the text the user was composing.
    ui.set_text("Misurata Hospital".into());
    slint::platform::update_timers_and_animations();
    assert_eq!(ui.get_composing(), "");
    assert_eq!(ui.get_text(), "Misurata Hospital");
}
