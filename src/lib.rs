#![deny(missing_docs)]
//! Base support for wasm service using Confluence Workers
//!
use async_trait::async_trait;
use js_sys::{Function, Reflect};
use std::{convert::Into, rc::Rc, sync::Mutex};
use wasm_bindgen::JsValue;

mod error;
pub use error::Error;
mod method;
pub use method::Method;
mod request;
pub use request::Request;
mod response;
pub use response::Response;

mod context;
pub use context::Context;
pub(crate) mod js_values;

use logging::{log, prelude::*};
pub(crate) use service_logging as logging;

/// Runnable trait for deferred tasks
#[async_trait(?Send)]
pub trait Runnable {
    /// Deferred tasks may not return values or errors. (they also shouldn't panic)
    /// Log messages appended to the log queue will be sent to the logger
    /// after all deferred tasks have run.
    async fn run(&self, lq: Rc<Mutex<logging::LogQueue>>);
}

/// Trait that defines app/service's request handler and router
#[async_trait(?Send)]
pub trait Handler<E> {
    /// Implementation of application request handler
    async fn handle(&self, ctx: &mut Context) -> Result<(), E>;
}

/// Entrypoint for wasm-service. Converts parameters from javascript,
/// invokes app-specific handler, and converts response to javascript.
/// Also sends logs to logger and runs deferred taskss
pub async fn service_request<E>(
    req: JsValue,
    logger: Box<dyn logging::Logger>,
    handler: Box<dyn Handler<E>>,
) -> Result<JsValue, JsValue>
where
    E: ToString,
{
    use js_sys::{Map, Object};
    let map = Map::from(req);
    let req = Request::from_js(&map)?;
    let js_event = Object::from(check_defined(
        map.get(&JsValue::from_str("event")),
        "missing event",
    )?);
    let mut ctx = Context::new(req);
    let response = match handler.handle(&mut ctx).await {
        Ok(_) => {
            let resp = ctx.take_response();
            let promise = deferred_promise(ctx.take_logs(), ctx.take_tasks(), logger);
            let wait_until =
                Function::from(Reflect::get(&js_event, &JsValue::from_str("waitUntil"))?);
            wait_until.call1(&js_event, &promise)?;
            resp
        }
        Err(e) => {
            // catch and log any errors that escaped handler
            log!(ctx, logging::Severity::Error, _:"handler", 
                        url: ctx.url().path(),
                        method: ctx.method(),
                        error: e);
            ctx.response()
                .status(500)
                .text("Sorry, unexpected internal error");
            ctx.take_response()
        }
    };
    Ok(response.into_js())
}

// Future task that will run deferred. Includes deferred logs plus user-defined tasks.
// This function contains a rust async wrapped in a Javascript Promise that will be passed
// to the event.waitUntil function, so it gets processed after response is returned.
fn deferred_promise(
    logs: Vec<logging::LogEntry>, // logs to send before deferred tasks are run
    tasks: Vec<Box<dyn Runnable + std::panic::UnwindSafe>>, // deferred tasks
    logger: Box<dyn logging::Logger>, // user's selected logger
) -> js_sys::Promise {
    wasm_bindgen_futures::future_to_promise(async move {
        let _ = logging::send_logs(logs, &logger).await;
        let lq = Rc::new(Mutex::new(logging::LogQueue::default()));
        for t in tasks.into_iter() {
            t.run(lq.clone()).await;
        }
        // if any logs were generated during processing of deferred tasks, send those
        let mut lock_queue = lq.lock().unwrap();
        let _ = logging::send_logs(lock_queue.take(), &logger).await;
        // all done, return nothing
        Ok(JsValue::undefined())
    })
}

// Returns javascript value, or Err if undefined
fn check_defined(v: JsValue, msg: &str) -> Result<JsValue, JsValue> {
    if v.is_undefined() {
        return Err(JsValue::from_str(msg.into()));
    }
    Ok(v)
}
