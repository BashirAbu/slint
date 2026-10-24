# Dftr changes

Every change this fork carries on top of upstream, newest first: what changed, why, the test that
holds it, and whether upstream has it (AGENTS.md §12 in github.com/BashirAbu/RustyDftr).

Base: upstream release tag `v1.18.1`.

| Commit | What changed | Why | Test | Upstream has it |
|---|---|---|---|---|
| `1112bf0` | Text: a line too long for its box that starts left of it, as Parley lays out an overflowing right-to-left line, is elided on the left: the glyphs left of the ellipsis's room are dropped and the ellipsis is drawn just left of the first kept glyph | `glyphs_with_elision` only cut on the right, so an overflowing Arabic title (a tab, a list cell, a page head) was drawn past its box's left edge, over the control beside it, with no ellipsis | `an_overflowing_right_to_left_line_is_elided_on_the_left` and `an_overflowing_left_to_right_line_is_elided_on_the_right` (`internal/core/textlayout/sharedparley/tests.rs`); Dftr's Arabic screenshots (`rendered_screens.rs`) | no |
| `2af0e64` | Android: a Back key press that no element takes is dispatched to the window as `WindowEvent::CloseRequested`; the key counts as handled when the window keeps itself shown | A key goes to the focused element and its ancestors only, so once the focused element was gone (a button that hid itself, a menu rebuilt in another language) no element took Back and Android's default finished the activity; Dftr's Back never quits (its docs/06 §4) | Dftr's `back_is_always_taken_so_it_never_quits` (headless: Back after the focused element went, through the close request) and `platform_phone_makes_a_join_code_…` (the phone: Back after Done hid itself and after a language switch) | no |
| `65cd9fe` | Android: a key event types the character its key character map gives for the key and its modifiers (Shift+`;` types `:`, Shift+`a` types `A`); keys that type nothing printable keep the fixed table | The fixed table ignored the meta state, so a hardware keyboard, and `adb shell input text`, typed `ws;//` for `ws://` | Dftr's `platform_phone_…` scenario types a server address with colons on the phone; the Android backend has no host-run tests, because key events only come from a running activity | no |
