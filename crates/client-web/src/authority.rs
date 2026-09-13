//! The session authority behind altc's web API.
//!
//! JavaScript owns the HTTP side — cookies, CSRF, the login — so the
//! authority is an object of four methods returning promises, and this
//! wrapper turns their results into the backend's own types. The shapes are
//! the backend's structs serialised as they are:
//!
//! - `serverInfo(): Promise<ServerInfo>`
//! - `launch(appId: number, mode: StreamMode): Promise<LaunchedSession>`
//! - `resume(mode: StreamMode): Promise<LaunchedSession>`
//! - `cancel(): Promise<void>`
//!
//! `LaunchedSession.tunnel_token` carries the token the tunnel presents in
//! its hello.

use crate::js_error;
use gsa_backend_moonlight::{LaunchedSession, ServerInfo, SessionAuthority, StreamMode};
use gsa_core::error::ProtocolError;
use gsa_core::{Error, Result};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// A [`SessionAuthority`] over a JavaScript object.
#[derive(Debug, Clone)]
pub struct JsAuthority {
    object: js_sys::Object,
}

impl JsAuthority {
    /// Wrap `object`, which must carry the four methods named in the module
    /// documentation; a missing one is reported at the call, not here, so
    /// the error names what was asked for.
    pub fn new(object: JsValue) -> Result<Self> {
        let object = object
            .dyn_into::<js_sys::Object>()
            .map_err(|_| Error::Auth("the authority is not an object".into()))?;
        Ok(Self { object })
    }

    /// Call `method` with `args`, await its promise, and deserialise.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        args: &[JsValue],
    ) -> Result<T> {
        let value = self.call_raw(method, args).await?;
        serde_wasm_bindgen::from_value(value)
            .map_err(|e| Error::Protocol(ProtocolError::Deserialize(format!("{method}: {e}"))))
    }

    async fn call_raw(&self, method: &str, args: &[JsValue]) -> Result<JsValue> {
        let function = js_sys::Reflect::get(&self.object, &JsValue::from_str(method))
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .ok_or_else(|| Error::Auth(format!("the authority has no `{method}` method")))?;
        let array = js_sys::Array::new();
        for arg in args {
            array.push(arg);
        }
        let returned = function
            .apply(&self.object, &array)
            .map_err(|e| js_error(method, &e))?;
        let promise = js_sys::Promise::resolve(&returned);
        JsFuture::from(promise)
            .await
            .map_err(|e| js_error(method, &e))
    }
}

fn mode_value(mode: StreamMode) -> Result<JsValue> {
    serde_wasm_bindgen::to_value(&mode)
        .map_err(|e| Error::Protocol(ProtocolError::Deserialize(format!("mode: {e}"))))
}

impl SessionAuthority for JsAuthority {
    async fn server_info(&self) -> Result<ServerInfo> {
        self.call("serverInfo", &[]).await
    }

    async fn launch(&self, app_id: u32, mode: StreamMode) -> Result<LaunchedSession> {
        self.call("launch", &[JsValue::from(app_id), mode_value(mode)?])
            .await
    }

    async fn resume(&self, mode: StreamMode) -> Result<LaunchedSession> {
        self.call("resume", &[mode_value(mode)?]).await
    }

    async fn cancel(&self) -> Result<()> {
        self.call_raw("cancel", &[]).await.map(|_| ())
    }
}
