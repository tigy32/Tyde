use protocol::TydeReleaseVersion;
use serde::Deserialize;
use wasm_bindgen::{JsCast, JsValue};

use crate::state::LocalHostId;

const SELECTION_KEY: &str = "tyde.selected-host.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    MissingRelease,
    Bridge,
    Storage,
    Manifest,
    Unpublished,
    Policy,
    Protocol,
    Integrity,
    Cancelled,
    ReloadLoop,
}

impl UnavailableReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingRelease => {
                "the host did not advertise a release (development or unpublished build)"
            }
            Self::Bridge => "this page has no compatible release loader",
            Self::Storage => "the browser could not persist the handoff",
            Self::Manifest => "the fresh release manifest could not be fetched",
            Self::Unpublished => "the host release has not been published",
            Self::Policy => "release policy disallows the exact bundle",
            Self::Protocol => "the published bundle does not match the host protocol",
            Self::Integrity => "the exact bundle failed integrity verification",
            Self::Cancelled => "the preparation was cancelled",
            Self::ReloadLoop => "repeated switching was stopped; the published bundle needs repair",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrowserOutcome {
    Ready { ticket: u32 },
    Unavailable { reason: UnavailableReason },
    Matching,
    Reloading,
}

fn call(method: &str, args: &[JsValue]) -> Result<JsValue, UnavailableReason> {
    let window = web_sys::window().ok_or(UnavailableReason::Bridge)?;
    let api = js_sys::Reflect::get(&window, &"__tydeLoader".into())
        .map_err(|_| UnavailableReason::Bridge)?;
    let function = js_sys::Reflect::get(&api, &method.into())
        .map_err(|_| UnavailableReason::Bridge)?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| UnavailableReason::Bridge)?;
    let array = js_sys::Array::new();
    for arg in args {
        array.push(arg);
    }
    function
        .apply(&api, &array)
        .map_err(|_| UnavailableReason::Bridge)
}

fn outcome(value: Result<JsValue, UnavailableReason>) -> BrowserOutcome {
    match value.and_then(|value| {
        serde_wasm_bindgen::from_value(value).map_err(|_| UnavailableReason::Bridge)
    }) {
        Ok(result) => result,
        Err(reason) => BrowserOutcome::Unavailable { reason },
    }
}

pub fn boot_version() -> Result<TydeReleaseVersion, UnavailableReason> {
    let value = call("bootVersion", &[])?
        .as_string()
        .ok_or(UnavailableReason::Bridge)?;
    TydeReleaseVersion::parse(&value).map_err(|_| UnavailableReason::Bridge)
}

pub fn cancel() -> Result<(), UnavailableReason> {
    call("cancelHostSwitch", &[]).map(|_| ())
}

pub async fn prepare(
    host: &LocalHostId,
    version: &TydeReleaseVersion,
    protocol: u32,
) -> BrowserOutcome {
    let value = call(
        "prepareHostSwitch",
        &[
            version.as_str().into(),
            protocol.into(),
            host.0.as_str().into(),
        ],
    );
    let result = match value {
        Ok(value) => match value.dyn_into::<js_sys::Promise>() {
            Ok(promise) => wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .map_err(|_| UnavailableReason::Bridge),
            Err(_) => Err(UnavailableReason::Bridge),
        },
        Err(reason) => Err(reason),
    };
    outcome(result)
}

pub fn commit(ticket: u32, host: &LocalHostId) -> BrowserOutcome {
    outcome(call(
        "commitHostSwitch",
        &[ticket.into(), host.0.as_str().into()],
    ))
}

pub fn confirm(host: &LocalHostId, version: &TydeReleaseVersion, protocol: u32) -> BrowserOutcome {
    outcome(call(
        "confirmHostRelease",
        &[
            version.as_str().into(),
            protocol.into(),
            host.0.as_str().into(),
        ],
    ))
}

fn storage() -> Result<web_sys::Storage, UnavailableReason> {
    web_sys::window()
        .ok_or(UnavailableReason::Storage)?
        .local_storage()
        .map_err(|_| UnavailableReason::Storage)?
        .ok_or(UnavailableReason::Storage)
}

pub fn read_selection() -> Result<Option<LocalHostId>, UnavailableReason> {
    storage()?
        .get_item(SELECTION_KEY)
        .map(|value| value.map(LocalHostId))
        .map_err(|_| UnavailableReason::Storage)
}

pub fn save_selection(host: Option<&LocalHostId>) -> Result<(), UnavailableReason> {
    persisted(call(
        "selectHost",
        &[host.map_or(JsValue::NULL, |host| host.0.as_str().into())],
    ))
}

pub fn record_release(
    host: &LocalHostId,
    version: Option<&TydeReleaseVersion>,
    protocol: u32,
) -> Result<(), UnavailableReason> {
    persisted(call(
        "recordHostRelease",
        &[
            version.map_or(JsValue::NULL, |version| version.as_str().into()),
            protocol.into(),
            host.0.as_str().into(),
        ],
    ))
}

fn persisted(value: Result<JsValue, UnavailableReason>) -> Result<(), UnavailableReason> {
    match outcome(value) {
        BrowserOutcome::Matching => Ok(()),
        BrowserOutcome::Unavailable { reason } => Err(reason),
        _ => Err(UnavailableReason::Bridge),
    }
}
