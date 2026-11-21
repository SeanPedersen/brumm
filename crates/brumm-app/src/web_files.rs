//! Browser file transfer: a hidden `<input type=file>` for picking files and a Blob download
//! for saving them. On desktop, `ui::files` uses native dialogs instead and this is empty.

#[cfg(target_family = "wasm")]
mod imp {
    use wasm_bindgen::{JsCast, closure::Closure};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, HtmlInputElement, Url};

    const INPUT_TYPE_FILE: &str = "file";
    const CHANGE_EVENT: &str = "change";
    const REVOKE_DELAY_MS: i32 = 60_000;
    const DEFAULT_MIME: &str = "application/octet-stream";

    /// Opens the browser's file chooser. `on_picked` gets the file name and bytes, or an error
    /// message when the browser could not read it. Nothing is called if the user cancels.
    /// Must run shortly after a user gesture, or the browser blocks the chooser.
    pub fn pick(
        accept: Option<&str>,
        on_picked: impl FnOnce(Result<(String, Vec<u8>), &'static str>) + 'static,
    ) {
        let Some(input) = create_input(accept) else {
            on_picked(Err("the browser could not open a file chooser"));
            return;
        };
        let reader = input.clone();
        let mut on_picked = Some(on_picked);
        let on_change = Closure::<dyn FnMut()>::new(move || {
            let Some(file) = reader.files().and_then(|files| files.get(0)) else {
                return;
            };
            let Some(on_picked) = on_picked.take() else {
                return;
            };
            wasm_bindgen_futures::spawn_local(async move {
                let name = file.name();
                let result = JsFuture::from(file.array_buffer())
                    .await
                    .map(|buffer| (name, js_sys::Uint8Array::new(&buffer).to_vec()))
                    .map_err(|_| "the browser could not read the file");
                on_picked(result);
            });
        });
        let _ = input
            .add_event_listener_with_callback(CHANGE_EVENT, on_change.as_ref().unchecked_ref());
        // The input is dropped by the browser after the chooser closes; the closure must outlive it.
        on_change.forget();
        input.click();
    }

    fn create_input(accept: Option<&str>) -> Option<HtmlInputElement> {
        let document = web_sys::window()?.document()?;
        let input: HtmlInputElement = document.create_element("input").ok()?.dyn_into().ok()?;
        input.set_type(INPUT_TYPE_FILE);
        if let Some(accept) = accept {
            input.set_accept(accept);
        }
        Some(input)
    }

    /// Hands `data` to the browser as a download named `name`.
    pub fn download(name: &str, data: &[u8]) {
        let _ = try_download(name, data);
    }

    fn try_download(name: &str, data: &[u8]) -> Option<()> {
        let bytes = js_sys::Uint8Array::from(data);
        let parts = js_sys::Array::of1(&bytes);
        let options = BlobPropertyBag::new();
        options.set_type(DEFAULT_MIME);
        let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
        let url = Url::create_object_url_with_blob(&blob).ok()?;
        let document = web_sys::window()?.document()?;
        let anchor: HtmlAnchorElement = document.create_element("a").ok()?.dyn_into().ok()?;
        anchor.set_href(&url);
        anchor.set_download(name);
        anchor.click();
        // Revoking at once can cancel the download in some browsers.
        let revoke = Closure::once_into_js(move || {
            let _ = Url::revoke_object_url(&url);
        });
        let _ = web_sys::window()?.set_timeout_with_callback_and_timeout_and_arguments_0(
            revoke.unchecked_ref(),
            REVOKE_DELAY_MS,
        );
        Some(())
    }
}

#[cfg(target_family = "wasm")]
pub use imp::{download, pick};

#[cfg(not(target_family = "wasm"))]
pub fn download(_name: &str, _data: &[u8]) {}
