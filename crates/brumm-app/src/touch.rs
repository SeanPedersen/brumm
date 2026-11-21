//! Phones and tablets (primary pointer is a finger). There, Enter on the on-screen keyboard
//! inserts a newline, and the send button is backed by a real DOM element: a tap on the canvas
//! makes the browser move focus away from the hidden text input, which closes the keyboard. A
//! transparent element that swallows the press keeps the input focused.

#[cfg(target_family = "wasm")]
mod imp {
    use std::{
        cell::RefCell,
        sync::{
            OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    use wasm_bindgen::{JsCast, closure::Closure};
    use web_sys::HtmlElement;

    const COARSE_POINTER_QUERY: &str = "(pointer: coarse)";
    const CANVAS_ID: &str = "brumm-canvas";
    const OVERLAY_Z_INDEX: &str = "10";
    const PRESS_EVENTS: [&str; 3] = ["pointerdown", "mousedown", "touchstart"];
    const RELEASE_EVENT: &str = "pointerup";

    static TAPPED: AtomicBool = AtomicBool::new(false);

    #[derive(Default)]
    struct Overlay {
        element: Option<HtmlElement>,
        /// What the frame asked for; applied once at the end of the frame.
        wanted: Option<egui::Rect>,
        placed: Option<egui::Rect>,
    }

    thread_local! {
        static OVERLAY: RefCell<Overlay> = RefCell::new(Overlay::default());
    }

    pub fn is_touch_device() -> bool {
        static COARSE: OnceLock<bool> = OnceLock::new();
        *COARSE.get_or_init(|| {
            web_sys::window()
                .and_then(|window| window.match_media(COARSE_POINTER_QUERY).ok().flatten())
                .is_some_and(|query| query.matches())
        })
    }

    pub fn begin_frame() {
        OVERLAY.with_borrow_mut(|overlay| overlay.wanted = None);
    }

    /// The send button is drawn at `rect` (canvas points) this frame.
    pub fn send_button_at(ctx: &egui::Context, rect: egui::Rect) {
        if !is_touch_device() {
            return;
        }
        OVERLAY.with_borrow_mut(|overlay| {
            if overlay.element.is_none() {
                overlay.element = create_element(ctx);
            }
            overlay.wanted = Some(rect);
        });
    }

    pub fn end_frame() {
        OVERLAY.with_borrow_mut(|overlay| {
            if overlay.wanted == overlay.placed {
                return;
            }
            let Some(element) = &overlay.element else {
                return;
            };
            apply(element, overlay.wanted);
            overlay.placed = overlay.wanted;
        });
    }

    pub fn take_send_tap() -> bool {
        TAPPED.swap(false, Ordering::Relaxed)
    }

    fn apply(element: &HtmlElement, rect: Option<egui::Rect>) {
        let style = element.style();
        let Some(rect) = rect else {
            let _ = style.set_property("display", "none");
            return;
        };
        let canvas_origin = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(CANVAS_ID))
            .map(|canvas| canvas.get_bounding_client_rect())
            .map_or((0.0, 0.0), |bounds| (bounds.left(), bounds.top()));
        let px = |value: f64| format!("{value}px");
        let _ = style.set_property("display", "block");
        let _ = style.set_property("left", &px(canvas_origin.0 + f64::from(rect.min.x)));
        let _ = style.set_property("top", &px(canvas_origin.1 + f64::from(rect.min.y)));
        let _ = style.set_property("width", &px(f64::from(rect.width())));
        let _ = style.set_property("height", &px(f64::from(rect.height())));
    }

    fn create_element(ctx: &egui::Context) -> Option<HtmlElement> {
        let document = web_sys::window()?.document()?;
        let element = document
            .create_element("div")
            .ok()?
            .dyn_into::<HtmlElement>()
            .ok()?;
        let style = element.style();
        for (name, value) in [
            ("position", "fixed"),
            ("display", "none"),
            ("z-index", OVERLAY_Z_INDEX),
            ("touch-action", "none"),
            ("background", "transparent"),
        ] {
            let _ = style.set_property(name, value);
        }
        for event in PRESS_EVENTS {
            let swallow = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
                event.prevent_default();
                event.stop_propagation();
            });
            element
                .add_event_listener_with_callback(event, swallow.as_ref().unchecked_ref())
                .ok()?;
            swallow.forget();
        }
        let ctx = ctx.clone();
        let tap = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            event.prevent_default();
            TAPPED.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        element
            .add_event_listener_with_callback(RELEASE_EVENT, tap.as_ref().unchecked_ref())
            .ok()?;
        tap.forget();
        document.body()?.append_child(&element).ok()?;
        Some(element)
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    pub fn is_touch_device() -> bool {
        false
    }

    pub fn begin_frame() {}

    pub fn send_button_at(_ctx: &egui::Context, _rect: egui::Rect) {}

    pub fn end_frame() {}

    pub fn take_send_tap() -> bool {
        false
    }
}

pub use imp::{begin_frame, end_frame, is_touch_device, send_button_at, take_send_tap};
