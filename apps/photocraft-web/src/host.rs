//! JavaScript bindings only; the CMS owns selection, identity and persistence.
use std::cell::RefCell;

use js_sys::{Function, Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

use photocraft_ui_egui::{PhotocraftApp, host_files::Action};

type Load = (String, Vec<u8>, Function, Function);
type Loading = (std::sync::mpsc::Receiver<Result<(), String>>, Function, Function);

#[derive(Default)]
struct Bridge {
    hooks: Option<JsValue>,
    ready: Option<Result<(), String>>,
    waiters: Vec<(Function, Function)>,
    loads: Vec<Load>,
    loading: Vec<Loading>,
    context: Option<egui::Context>,
}

thread_local! { static BRIDGE: RefCell<Bridge> = RefCell::default(); }

fn error(value: JsValue) -> String {
    Reflect::get(&value, &"message".into()).ok().and_then(|v| v.as_string()).or_else(|| value.as_string()).unwrap_or_else(|| format!("{value:?}"))
}

fn method(hooks: &JsValue, name: &str) -> Result<Function, JsValue> {
    Reflect::get(hooks, &name.into())?.dyn_into().map_err(|_| JsValue::from_str(&format!("Host integration needs an async {name}() function")))
}

/// Configure before awaiting ready(). null leaves standalone browser services in place.
#[wasm_bindgen(js_name = setHostIntegration)]
pub fn set_host_integration(hooks: JsValue) -> Result<(), JsValue> {
    if !hooks.is_null() && !hooks.is_undefined() {
        method(&hooks, "open")?;
        method(&hooks, "save")?;
    }
    BRIDGE.with_borrow_mut(|b| {
        if b.hooks.is_some() {
            return Err(JsValue::from_str("Host integration is already configured; reload to change integration"));
        }
        b.hooks = (!hooks.is_null() && !hooks.is_undefined()).then_some(hooks);
        Ok(())
    })
}

pub fn configured() -> bool {
    BRIDGE.with_borrow(|b| b.hooks.is_some())
}

/// Stable entry point shared by Trunk's direct loader and the packaged boot loader.
pub fn publish() -> Result<(), JsValue> {
    use wasm_bindgen::closure::Closure;
    let api = js_sys::Object::new();
    let configure = Closure::<dyn FnMut(JsValue) -> Result<(), JsValue>>::new(set_host_integration).into_js_value();
    let load = Closure::<dyn FnMut(JsValue) -> Promise>::new(|file| {
        wasm_bindgen_futures::future_to_promise(async move { load_document(file).await.map(|_| JsValue::UNDEFINED) })
    })
    .into_js_value();
    let connect = Closure::<dyn FnMut(String) -> Result<(), JsValue>>::new(super::iframe::browser::connect_parent).into_js_value();
    Reflect::set(&api, &"ready".into(), &ready())?;
    Reflect::set(&api, &"setHostIntegration".into(), &configure)?;
    Reflect::set(&api, &"loadDocument".into(), &load)?;
    Reflect::set(&api, &"connectParent".into(), &connect)?;
    let window = web_sys::window().ok_or("no window")?;
    Reflect::set(&window, &"PhotoCraft".into(), &api)?;
    window.dispatch_event(&web_sys::Event::new("photocraft-api-ready")?)?;
    Ok(())
}

/// Resolves after WebRunner startup, rejects if startup fails.
#[wasm_bindgen]
pub fn ready() -> Promise {
    Promise::new(&mut |resolve, reject| {
        BRIDGE.with_borrow_mut(|b| match &b.ready {
            Some(Ok(())) => {
                let _ = resolve.call0(&JsValue::UNDEFINED);
            }
            Some(Err(e)) => {
                let _ = reject.call1(&JsValue::UNDEFINED, &e.clone().into());
            }
            None => b.waiters.push((resolve, reject)),
        });
    })
}

pub fn started(result: Result<(), String>) {
    let waiters = BRIDGE.with_borrow_mut(|b| {
        b.ready = Some(result.clone());
        std::mem::take(&mut b.waiters)
    });
    for (resolve, reject) in waiters {
        match &result {
            Ok(()) => {
                let _ = resolve.call0(&JsValue::UNDEFINED);
            }
            Err(e) => {
                let _ = reject.call1(&JsValue::UNDEFINED, &e.clone().into());
            }
        }
    }
}

pub fn attach(ctx: egui::Context) {
    BRIDGE.with_borrow_mut(|b| b.context = Some(ctx));
}

fn read_file(file: &JsValue) -> Result<(String, Vec<u8>), JsValue> {
    let name = Reflect::get(file, &"name".into())?
        .as_string()
        .filter(|n| !n.is_empty() && n.len() <= 4096 && !n.chars().any(char::is_control))
        .ok_or("EditorFile.name must be a nonempty filename (at most 4096 bytes)")?;
    let bytes = Reflect::get(file, &"bytes".into())?.dyn_into::<Uint8Array>().map_err(|_| "EditorFile.bytes must be a Uint8Array")?;
    if bytes.length() == 0 || bytes.length() > 256 * 1024 * 1024 {
        return Err("EditorFile.bytes must contain 1–268435456 bytes".into());
    }
    Ok((name, bytes.to_vec()))
}

/// Waits for readiness, then submits once through the same decoder as File › Open.
#[wasm_bindgen(js_name = loadDocument)]
pub async fn load_document(file: JsValue) -> Result<(), JsValue> {
    let (name, bytes) = read_file(&file)?;
    JsFuture::from(ready()).await?;
    let promise = Promise::new(&mut |resolve, reject| {
        BRIDGE.with_borrow_mut(|b| {
            b.loads.push((name.clone(), bytes.clone(), resolve, reject));
            if let Some(ctx) = &b.context {
                ctx.request_repaint();
            }
        });
    });
    JsFuture::from(promise).await.map(|_| ())
}

pub fn poll(app: &mut PhotocraftApp) {
    let loads = BRIDGE.with_borrow_mut(|b| std::mem::take(&mut b.loads));
    for (name, bytes, resolve, reject) in loads {
        match app.open_host_document(&name, &bytes) {
            Ok(receiver) => BRIDGE.with_borrow_mut(|b| b.loading.push((receiver, resolve, reject))),
            Err(e) => {
                app.ui.status = format!("Could not load {name}: {e}");
                app.ui.status_error = true;
                let _ = reject.call1(&JsValue::UNDEFINED, &app.ui.status.clone().into());
            }
        }
    }
    let loading = BRIDGE.with_borrow_mut(|b| std::mem::take(&mut b.loading));
    for (receiver, resolve, reject) in loading {
        match receiver.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => BRIDGE.with_borrow_mut(|b| b.loading.push((receiver, resolve, reject))),
            result => {
                let result = result.unwrap_or_else(|_| Err("Document loading ended without a result".into()));
                match result {
                    Ok(()) => {
                        let _ = resolve.call0(&JsValue::UNDEFINED);
                    }
                    Err(e) => {
                        let _ = reject.call1(&JsValue::UNDEFINED, &e.into());
                    }
                }
            }
        }
    }
    BRIDGE.with_borrow(|b| {
        if !b.loading.is_empty()
            && let Some(ctx) = &b.context
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    });
}

pub async fn open() -> Result<Option<(String, Vec<u8>)>, String> {
    let hooks = BRIDGE.with_borrow(|b| b.hooks.clone()).ok_or("Host integration is not configured")?;
    let open = method(&hooks, "open").map_err(error)?;
    let result = open.call0(&hooks).map_err(error)?;
    let file = JsFuture::from(Promise::resolve(&result)).await.map_err(error)?;
    if file.is_null() { Ok(None) } else { read_file(&file).map(Some).map_err(error) }
}

pub async fn save(name: String, bytes: Vec<u8>, action: Action) -> Result<(), String> {
    let hooks = BRIDGE.with_borrow(|b| b.hooks.clone()).ok_or("Host integration is not configured")?;
    let file = js_sys::Object::new();
    Reflect::set(&file, &"name".into(), &name.into()).map_err(error)?;
    Reflect::set(&file, &"bytes".into(), &Uint8Array::from(bytes.as_slice())).map_err(error)?;
    let result = method(&hooks, "save").map_err(error)?.call2(&hooks, &file, &action.as_str().into()).map_err(error)?;
    // Require explicit asynchronous acknowledgement, not an accidental undefined return.
    let promise = result.dyn_into::<Promise>().map_err(|_| "Host save() must return a Promise")?;
    JsFuture::from(promise).await.map(|_| ()).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    fn reset() {
        BRIDGE.with_borrow_mut(|b| *b = Bridge::default());
    }

    #[wasm_bindgen_test]
    async fn readiness_waits_then_resolves_and_repeated_waiters_share_the_result() {
        reset();
        let first = ready();
        let second = ready();
        assert_eq!(BRIDGE.with_borrow(|b| b.waiters.len()), 2);
        started(Ok(()));
        assert!(JsFuture::from(first).await.is_ok());
        assert!(JsFuture::from(second).await.is_ok());
        assert!(JsFuture::from(ready()).await.is_ok());
    }

    #[wasm_bindgen_test]
    async fn startup_failure_rejects_readiness_and_loads() {
        reset();
        let waiting = ready();
        started(Err("WebGPU and WebGL2 are unavailable".into()));
        assert!(JsFuture::from(waiting).await.is_err());
        assert!(JsFuture::from(ready()).await.is_err());
        let file = js_sys::Object::new();
        Reflect::set(&file, &"name".into(), &"initial.png".into()).unwrap();
        Reflect::set(&file, &"bytes".into(), &Uint8Array::from(&b"file"[..])).unwrap();
        assert!(load_document(file.into()).await.is_err());
        assert!(BRIDGE.with_borrow(|b| b.loads.is_empty()));
    }

    #[wasm_bindgen_test]
    async fn initial_load_is_queued_once_and_only_after_readiness() {
        reset();
        let file = js_sys::Object::new();
        Reflect::set(&file, &"name".into(), &"initial.png".into()).unwrap();
        Reflect::set(&file, &"bytes".into(), &Uint8Array::from(&b"document"[..])).unwrap();
        let loading = wasm_bindgen_futures::future_to_promise(async move { load_document(file.into()).await.map(|_| JsValue::UNDEFINED) });
        // Let the submitted future reach its readiness wait.
        JsFuture::from(Promise::resolve(&JsValue::UNDEFINED)).await.unwrap();
        assert!(BRIDGE.with_borrow(|b| b.loads.is_empty()));
        started(Ok(()));
        JsFuture::from(Promise::resolve(&JsValue::UNDEFINED)).await.unwrap();
        let queued = BRIDGE.with_borrow_mut(|b| std::mem::take(&mut b.loads));
        assert_eq!(queued.len(), 1);
        let (name, bytes, resolve, _) = queued.into_iter().next().unwrap();
        assert_eq!(name, "initial.png");
        assert_eq!(bytes, b"document");
        resolve.call0(&JsValue::UNDEFINED).unwrap();
        assert!(JsFuture::from(loading).await.is_ok());
        assert!(BRIDGE.with_borrow(|b| b.loads.is_empty()));
    }

    #[wasm_bindgen_test]
    fn invalid_input_is_rejected_before_allocating_or_queuing_a_load() {
        reset();
        assert!(read_file(&JsValue::NULL).is_err());
        let file = js_sys::Object::new();
        Reflect::set(&file, &"name".into(), &"image.png".into()).unwrap();
        Reflect::set(&file, &"bytes".into(), &js_sys::Array::new()).unwrap();
        assert!(read_file(&file).is_err());
        Reflect::set(&file, &"bytes".into(), &Uint8Array::new_with_length(0)).unwrap();
        assert!(read_file(&file).is_err());
        Reflect::set(&file, &"name".into(), &"bad\nname.png".into()).unwrap();
        assert!(read_file(&file).is_err());
    }

    #[wasm_bindgen_test]
    async fn picker_cancellation_and_upload_rejection_cross_the_js_boundary() {
        reset();
        let hooks = js_sys::Object::new();
        let open = wasm_bindgen::closure::Closure::<dyn FnMut() -> Promise>::new(|| Promise::resolve(&JsValue::NULL)).into_js_value();
        let save = wasm_bindgen::closure::Closure::<dyn FnMut(JsValue, JsValue) -> Promise>::new(|_: JsValue, action: JsValue| {
            assert_eq!(action.as_string().as_deref(), Some("saveAs"));
            Promise::reject(&"Storage permission denied".into())
        })
        .into_js_value();
        Reflect::set(&hooks, &"open".into(), &open).unwrap();
        Reflect::set(&hooks, &"save".into(), &save).unwrap();
        set_host_integration(hooks.into()).unwrap();
        assert!(super::open().await.unwrap().is_none());
        let result = super::save("layers.pcraft".into(), vec![1], Action::SaveAs).await;
        assert_eq!(result, Err("Storage permission denied".into()));
    }
}
