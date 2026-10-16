//! Opt-in, origin- and source-bound iframe file adapter.

const CHANNEL: &str = "photocraft.files.v1";

fn request_id(id: &str) -> Option<u64> {
    if id.is_empty() || id.len() > 16 || id.starts_with('0') || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse::<u64>().ok().filter(|n| *n <= 9_007_199_254_740_991)
}

fn trusted_sender(expected_origin: &str, origin: &str, from_parent: bool) -> bool {
    from_parent && origin == expected_origin
}

fn fresh_request(id: &str, last: &mut u64) -> bool {
    let Some(n) = request_id(id).filter(|n| *n > *last) else { return false };
    *last = n;
    true
}

#[cfg(target_arch = "wasm32")]
pub(crate) mod browser {
    use js_sys::{Function, Object, Promise, Reflect};
    use std::{cell::RefCell, collections::HashMap};
    use wasm_bindgen::{JsCast, JsValue, closure::Closure, prelude::wasm_bindgen};
    use wasm_bindgen_futures::{JsFuture, spawn_local};

    use super::{CHANNEL, fresh_request, request_id, trusted_sender};

    struct Connection {
        parent: web_sys::Window,
        origin: String,
        next: u64,
        last_request: u64,
        pending: HashMap<String, (Function, Function)>,
    }
    thread_local! { static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) }; }

    fn get(value: &JsValue, key: &str) -> Option<JsValue> {
        Reflect::get(value, &key.into()).ok()
    }
    fn string(value: &JsValue, key: &str) -> Option<String> {
        get(value, key)?.as_string()
    }
    fn set(value: &Object, key: &str, data: &JsValue) -> Result<(), JsValue> {
        Reflect::set(value, &key.into(), data).map(|_| ())
    }
    fn envelope(kind: &str, id: &str) -> Result<Object, JsValue> {
        let message = Object::new();
        set(&message, "channel", &CHANNEL.into())?;
        set(&message, "kind", &kind.into())?;
        set(&message, "id", &id.into())?;
        Ok(message)
    }
    fn send(message: &JsValue) -> Result<(), JsValue> {
        CONNECTION.with_borrow(|c| {
            let c = c.as_ref().ok_or("Iframe adapter is not connected")?;
            c.parent.post_message(message, &c.origin)
        })
    }
    fn settle(id: &str, result: Result<JsValue, JsValue>) {
        let callbacks = CONNECTION.with_borrow_mut(|c| c.as_mut().and_then(|c| c.pending.remove(id)));
        if let Some((resolve, reject)) = callbacks {
            match result {
                Ok(value) => {
                    let _ = resolve.call1(&JsValue::UNDEFINED, &value);
                }
                Err(error) => {
                    let _ = reject.call1(&JsValue::UNDEFINED, &error);
                }
            }
        }
    }
    fn request(method: &str, file: JsValue, action: JsValue) -> Promise {
        Promise::new(&mut |resolve, reject| {
            let id = CONNECTION.with_borrow_mut(|c| {
                let c = c.as_mut().ok_or("Iframe adapter is not connected")?;
                if c.pending.len() >= 16 {
                    return Err("Too many pending CMS requests");
                }
                c.next = c.next.checked_add(1).filter(|n| *n <= 9_007_199_254_740_991).ok_or("Reload to renew iframe request IDs")?;
                let id = c.next.to_string();
                c.pending.insert(id.clone(), (resolve.clone(), reject.clone()));
                Ok(id)
            });
            let id = match id {
                Ok(id) => id,
                Err(e) => {
                    let _ = reject.call1(&JsValue::UNDEFINED, &e.into());
                    return;
                }
            };
            let sent = (|| {
                let message = envelope("request", &id)?;
                set(&message, "method", &method.into())?;
                set(&message, "file", &file)?;
                set(&message, "action", &action)?;
                send(&message)
            })();
            if let Err(e) = sent {
                settle(&id, Err(e));
                return;
            }
            // A host that disappears must not leave Save waiting forever.
            let timeout_id = id.clone();
            let timeout = Closure::once_into_js(move || settle(&timeout_id, Err("CMS request timed out after 120 seconds; retry the action".into())));
            let timer = web_sys::window()
                .ok_or_else(|| JsValue::from_str("no window"))
                .and_then(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(timeout.unchecked_ref(), 120_000));
            if let Err(e) = timer {
                settle(&id, Err(e));
            }
        })
    }

    fn receive(event: web_sys::MessageEvent) {
        let trusted = CONNECTION.with_borrow(|c| {
            c.as_ref().is_some_and(|c| trusted_sender(&c.origin, &event.origin(), get(event.as_ref(), "source").as_ref() == Some(c.parent.as_ref())))
        });
        if !trusted {
            return;
        }
        let data = event.data();
        if string(&data, "channel").as_deref() != Some(CHANNEL) {
            return;
        }
        let Some(id) = string(&data, "id").filter(|s| request_id(s).is_some()) else { return };
        match string(&data, "kind").as_deref() {
            Some("response") => {
                let result = match get(&data, "ok").and_then(|v| v.as_bool()) {
                    Some(true) => Ok(get(&data, "value").unwrap_or(JsValue::UNDEFINED)),
                    Some(false) => Err(string(&data, "error").unwrap_or_else(|| "CMS request failed".into()).into()),
                    None => Err("Invalid CMS response: expected boolean ok".into()),
                };
                settle(&id, result);
            }
            Some("request") => {
                let fresh = CONNECTION.with_borrow_mut(|c| {
                    let Some(c) = c.as_mut() else { return false };
                    fresh_request(&id, &mut c.last_request)
                });
                if !fresh {
                    return;
                }
                spawn_local(async move {
                    let result = match string(&data, "method").as_deref() {
                        Some("ready") => JsFuture::from(crate::host::ready()).await.map(|_| JsValue::UNDEFINED),
                        Some("loadDocument") => crate::host::load_document(get(&data, "file").unwrap_or(JsValue::UNDEFINED)).await.map(|_| JsValue::UNDEFINED),
                        _ => Err("Unknown PhotoCraft iframe method".into()),
                    };
                    let reply = (|| {
                        let reply = envelope("response", &id)?;
                        set(&reply, "ok", &result.is_ok().into())?;
                        match result {
                            Ok(value) => set(&reply, "value", &value)?,
                            Err(e) => set(&reply, "error", &format!("{e:?}").into())?,
                        }
                        send(&reply)
                    })();
                    if let Err(e) = reply {
                        log::error!("Could not reply to iframe request: {e:?}");
                    }
                });
            }
            _ => {}
        }
    }

    /// Opt-in only. The host origin is trusted configuration, never read from a query or message.
    #[wasm_bindgen(js_name = connectParent)]
    pub fn connect_parent(origin: String) -> Result<(), JsValue> {
        let url = web_sys::Url::new(&origin)?;
        if !matches!(url.protocol().as_str(), "https:" | "http:") || url.origin() != origin {
            return Err("Provide an explicit HTTP(S) origin, without a path or trailing slash".into());
        }
        if CONNECTION.with_borrow(|c| c.is_some()) {
            return Err("Iframe adapter already connected".into());
        }
        let window = web_sys::window().ok_or("no window")?;
        let parent = window.parent()?.filter(|p| p != &window).ok_or("PhotoCraft must be inside an iframe")?;
        let hooks = Object::new();
        let open = Closure::<dyn FnMut() -> Promise>::new(|| request("open", JsValue::UNDEFINED, JsValue::UNDEFINED)).into_js_value();
        let save = Closure::<dyn FnMut(JsValue, JsValue) -> Promise>::new(|file, action| request("save", file, action)).into_js_value();
        set(&hooks, "open", &open)?;
        set(&hooks, "save", &save)?;
        crate::host::set_host_integration(hooks.into())?;
        let listener = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(receive);
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref())?;
        listener.forget();
        CONNECTION.with_borrow_mut(|c| *c = Some(Connection { parent, origin, next: 0, last_request: 0, pending: HashMap::new() }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_origin_source_and_replayed_requests_are_refused() {
        assert!(trusted_sender("https://cms.example", "https://cms.example", true));
        for origin in ["null", "http://cms.example", "https://cms.example.evil", "https://evil.example"] {
            assert!(!trusted_sender("https://cms.example", origin, true));
        }
        assert!(!trusted_sender("https://cms.example", "https://cms.example", false));
        let mut last = 0;
        assert!(fresh_request("1", &mut last));
        assert!(!fresh_request("1", &mut last));
        assert!(fresh_request("3", &mut last));
        assert!(!fresh_request("2", &mut last));
        assert!(!fresh_request("malformed", &mut last));
        assert_eq!(last, 3);
    }

    #[test]
    fn ids_are_canonical_positive_safe_integers() {
        assert_eq!(CHANNEL, "photocraft.files.v1");
        for id in ["1", "12", "9007199254740991"] {
            assert!(request_id(id).is_some(), "{id}");
        }
        for id in ["", "0", "01", "-1", "1.0", "NaN", "a", "9007199254740992", "18446744073709551616"] {
            assert!(request_id(id).is_none(), "{id}");
        }
    }
}
