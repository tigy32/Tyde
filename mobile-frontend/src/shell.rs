use std::{cell::RefCell, rc::Rc};

use leptos::prelude::*;
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen(module = "/vendor/web-shell/shell.js")]
extern "C" {
    type ShellHandle;

    #[wasm_bindgen(catch, js_name = attachShell)]
    fn attach_shell(root: &web_sys::HtmlElement, options: &JsValue)
    -> Result<ShellHandle, JsValue>;

    #[wasm_bindgen(method)]
    fn destroy(this: &ShellHandle);

    #[wasm_bindgen(catch, js_name = attachTextarea)]
    fn attach_textarea(
        textarea: &web_sys::HtmlTextAreaElement,
    ) -> Result<js_sys::Function, JsValue>;
}

struct MountedShell {
    handle: ShellHandle,
    regions: [web_sys::Element; 3],
}

impl Drop for MountedShell {
    fn drop(&mut self) {
        self.handle.destroy();
    }
}

fn region(root: &web_sys::HtmlElement, selector: &str) -> Result<web_sys::Element, JsValue> {
    root.query_selector(selector)?
        .ok_or_else(|| JsValue::from_str(&format!("Mobile shell missing {selector}")))
}

fn bind_regions(
    root: &web_sys::HtmlElement,
    mounted: &RefCell<Option<MountedShell>>,
) -> Result<(), JsValue> {
    let content = region(root, ".chat-messages, .view-body, .home-view")?;
    let header = root
        .query_selector(".shell-header, .chat-header, .view-header:not(.shell-header > *)")?
        .unwrap_or(region(root, ".shell-empty-header")?);
    let bottom = root
        .query_selector(".chat-bottom-dock, .shell-bottom")?
        .unwrap_or(region(root, ".shell-empty-bottom")?);
    let flow = content
        .query_selector(":scope > .shell-flow")?
        .ok_or_else(|| JsValue::from_str("Mobile shell missing content flow"))?;
    let regions = [header, content, bottom];
    if mounted
        .borrow()
        .as_ref()
        .is_some_and(|shell| shell.regions == regions)
    {
        return Ok(());
    }
    // Navigation replaces regions; keyboard changes never replace their DOM.
    mounted.borrow_mut().take();
    for (element, class) in regions
        .iter()
        .zip(["tws-header", "tws-content", "tws-bottom"])
    {
        element.class_list().add_1(class)?;
    }
    let options = js_sys::Object::new();
    let bindings = js_sys::Object::new();
    for (key, element) in [
        ("header", &regions[0]),
        ("content", &regions[1]),
        ("bottom", &regions[2]),
        ("flow", &flow),
    ] {
        js_sys::Reflect::set(&bindings, &key.into(), element)?;
    }
    js_sys::Reflect::set(&options, &"regions".into(), &bindings)?;
    js_sys::Reflect::set(&options, &"followEnd".into(), &false.into())?;
    let handle = attach_shell(root, &options)?;
    log::debug!("Shared mobile shell attached to mounted regions; followEnd=false");
    *mounted.borrow_mut() = Some(MountedShell { handle, regions });
    Ok(())
}

pub fn install(root: NodeRef<leptos::html::Div>) {
    Effect::new(move |_| {
        let Some(root) = root.get() else { return };
        let root: web_sys::HtmlElement = (*root).clone();
        let mounted = Rc::new(RefCell::new(None));
        let target = root.clone();
        let owner = mounted.clone();
        let callback = Closure::<dyn FnMut()>::new(move || {
            if let Err(error) = bind_regions(&target, &owner) {
                log::error!("Shared mobile shell could not bind regions: {error:?}");
            }
        });
        let observer = web_sys::MutationObserver::new(callback.as_ref().unchecked_ref())
            .expect("mobile shell navigation observer");
        let options = web_sys::MutationObserverInit::new();
        options.set_child_list(true);
        options.set_subtree(true);
        observer
            .observe_with_options(&root, &options)
            .expect("observe mobile shell");
        if let Err(error) = bind_regions(&root, &mounted) {
            log::error!("Shared mobile shell could not mount: {error:?}");
        }
        let held = send_wrapper::SendWrapper::new((observer, callback, mounted));
        on_cleanup(move || {
            let (observer, callback, mounted) = held.take();
            observer.disconnect();
            drop(callback);
            mounted.borrow_mut().take();
        });
    });
}

pub fn install_textarea(textarea: NodeRef<leptos::html::Textarea>) {
    Effect::new(move |_| {
        let Some(textarea) = textarea.get() else {
            return;
        };
        match attach_textarea(&textarea) {
            Ok(cleanup) => {
                let cleanup = send_wrapper::SendWrapper::new(cleanup);
                on_cleanup(move || {
                    let _ = cleanup.take().call0(&JsValue::UNDEFINED);
                });
            }
            Err(error) => log::error!("Shared textarea sizing could not mount: {error:?}"),
        }
    });
}
