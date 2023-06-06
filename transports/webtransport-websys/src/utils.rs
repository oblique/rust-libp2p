use js_sys::{Promise, Reflect};
use send_wrapper::SendWrapper;
use std::io;
use wasm_bindgen::{JsCast, JsValue};

use crate::Error;

/// Properly detach a promise.
///
/// A promise always runs in the background, however if you don't await it,
/// or specify a `catch` handler before you drop it, it might cause some side
/// effects. This function avoids any side effects.
///
/// Ref: https://github.com/typescript-eslint/typescript-eslint/blob/391a6702c0a9b5b3874a7a27047f2a721f090fb6/packages/eslint-plugin/docs/rules/no-floating-promises.md
pub(crate) fn detach_promise(promise: Promise) {
    type Closure = wasm_bindgen::closure::Closure<dyn FnMut(JsValue)>;
    static mut DO_NOTHING: Option<SendWrapper<Closure>> = None;

    // Allocate Closure only once and reuse it
    let do_nothing = unsafe {
        if DO_NOTHING.is_none() {
            let cb = Closure::new(|_| {});
            DO_NOTHING = Some(SendWrapper::new(cb));
        }

        DO_NOTHING.as_deref().unwrap()
    };

    // Avoid having "floating" promise and ignore any errors.
    // After `catch` promise is allowed to be dropped.
    let _ = promise.catch(do_nothing);
}

/// Typecasts a JavaScript type.
///
/// Returns a `Ok(value)` casted to the requested type.
///
/// If the underlying value is an error and the requested
/// type is not, then `Err(Error::JsError)` is returned.
///
/// If the underlying value can not be casted to the requested type and
/// is not an error, then `Err(Error::JsCastFailed)` is returned.
pub(crate) fn to_js_type<T>(value: impl Into<JsValue>) -> Result<T, Error>
where
    T: JsCast + From<JsValue>,
{
    let value = value.into();

    if value.has_type::<T>() {
        Ok(value.unchecked_into())
    } else if value.has_type::<js_sys::Error>() {
        Err(Error::from_js_value(value))
    } else {
        Err(Error::JsCastFailed)
    }
}

/// Parse reponse from `ReadableStreamDefaultReader::read`.
///
/// Ref: https://streams.spec.whatwg.org/#default-reader-prototype
pub(crate) fn parse_reader_response(resp: &JsValue) -> Result<Option<JsValue>, JsValue> {
    let value = Reflect::get(resp, &JsValue::from_str("value"))?;
    let done = Reflect::get(resp, &JsValue::from_str("done"))?
        .as_bool()
        .unwrap_or_default();

    if value.is_undefined() || done {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

pub(crate) fn to_io_error(value: JsValue) -> io::Error {
    io::Error::new(io::ErrorKind::Other, Error::from_js_value(value))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use js_sys::{Promise, TypeError, Uint8Array};
    use libp2p_core::Multiaddr;
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::wasm_bindgen_test;
    use web_sys::{window, Response, Window};

    #[wasm_bindgen_test]
    fn check_js_typecasting() {
        // Successful typecast.
        let value = JsValue::from(Uint8Array::new_with_length(0));
        assert!(to_js_type::<Uint8Array>(value).is_ok());

        // Type can not be typecasted.
        let value = JsValue::from(Uint8Array::new_with_length(0));
        assert!(matches!(
            to_js_type::<Promise>(value),
            Err(Error::JsCastFailed)
        ));

        // Request typecasting, however the underlying value is an error.
        let value = JsValue::from(TypeError::new("abc"));
        assert!(matches!(
            to_js_type::<Promise>(value),
            Err(Error::JsError(_))
        ));

        // Explicitly request js_sys::Error typecasting.
        let value = JsValue::from(TypeError::new("abc"));
        assert!(to_js_type::<js_sys::Error>(value).is_ok());
    }

    /// Helper that returns the multiaddress of echo-server
    ///
    /// It fetches the multiaddress via HTTP request to
    /// 127.0.0.1:4455.
    ///
    /// This is needed for test cases in other modules
    pub(crate) async fn fetch_server_addr() -> Multiaddr {
        let url = "http://127.0.0.1:4455/";
        let window = window().expect("failed to get browser window");

        let value = JsFuture::from(window.fetch_with_str(url))
            .await
            .expect("fetch failed");
        let resp = to_js_type::<Response>(value).expect("cast failed");

        let text = resp.text().expect("text failed");
        let text = JsFuture::from(text).await.expect("text promise failed");

        text.as_string()
            .filter(|s| !s.is_empty())
            .expect("response not a text")
            .parse()
            .unwrap()
    }
}
