// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! A press dispatched to a window, as a system test's is, has no cursor behind it: the real cursor
//! leaving the window, as when another window opens under it, must not end that press. Only
//! Windows lets a test make the cursor leave, by the message it sends when it does.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use slint::LogicalPosition;
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::{CustomApplicationHandler, EventResult, WinitWindowAccessor, winit};

    const WM_MOUSELEAVE: u32 = 0x02A3;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn PostMessageW(window: isize, message: u32, wparam: usize, lparam: isize) -> i32;
    }

    slint::slint! {
        export component App inherits Window {
            width: 100px;
            height: 100px;
            in-out property <int> clicks;
            TouchArea {
                clicked => { root.clicks += 1; }
            }
        }
    }

    /// Notes that the window has heard the cursor leave.
    struct NotesCursorLeft(Rc<Cell<bool>>);

    impl CustomApplicationHandler for NotesCursorLeft {
        fn window_event(
            &mut self,
            _event_loop: &winit::event_loop::ActiveEventLoop,
            _window_id: winit::window::WindowId,
            _winit_window: Option<&winit::window::Window>,
            _slint_window: Option<&slint::Window>,
            event: &winit::event::WindowEvent,
        ) -> EventResult {
            if matches!(event, winit::event::WindowEvent::CursorLeft { .. }) {
                self.0.set(true);
            }
            EventResult::Propagate
        }
    }

    let left = Rc::new(Cell::new(false));
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_custom_application_handler(NotesCursorLeft(left.clone()))
        .select()
        .unwrap();

    let app = App::new().unwrap();
    app.show().unwrap();
    let position = LogicalPosition::new(50., 50.);
    let button = PointerEventButton::Left;

    let pressing = app.as_weak();
    slint::spawn_local(async move {
        let app = pressing.unwrap();
        let winit_window = app.window().winit_window().await.unwrap();
        let RawWindowHandle::Win32(handle) = winit_window.window_handle().unwrap().as_raw() else {
            unreachable!("a window on Windows has a Win32 handle")
        };
        app.window().dispatch_event(WindowEvent::PointerMoved { position });
        app.window().dispatch_event(WindowEvent::PointerPressed { position, button });
        // SAFETY: the message carries no pointers, and the window outlives this event loop.
        let posted = unsafe { PostMessageW(handle.hwnd.get(), WM_MOUSELEAVE, 0, 0) };
        assert_ne!(posted, 0, "the message could not be posted");
    })
    .unwrap();

    // The event loop takes a timer in a later turn than the event the handler noted, so by then
    // the window has acted on the cursor leaving.
    let releasing = app.as_weak();
    let heard = left.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(1), move || {
        if heard.get() {
            let released = WindowEvent::PointerReleased { position, button };
            releasing.unwrap().window().dispatch_event(released);
            slint::quit_event_loop().unwrap();
        }
    });

    slint::run_event_loop().unwrap();
    assert!(left.get(), "the window never heard the cursor leave");
    assert_eq!(app.get_clicks(), 1, "the cursor leaving ended the dispatched press");
}
