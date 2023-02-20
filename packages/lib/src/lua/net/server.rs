use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use mlua::prelude::*;

use hyper::{body::to_bytes, server::conn::AddrStream, service::Service};
use hyper::{Body, Request, Response};
use hyper_tungstenite::{is_upgrade_request as is_ws_upgrade_request, upgrade as ws_upgrade};
use tokio::task;

use crate::{
    lua::task::{TaskScheduler, TaskSchedulerAsyncExt, TaskSchedulerScheduleExt},
    utils::table::TableBuilder,
};

use super::NetWebSocket;

// Hyper service implementation for net, lots of boilerplate here
// but make_svc and make_svc_function do not work for what we need

pub struct NetServiceInner(
    &'static Lua,
    Arc<LuaRegistryKey>,
    Arc<Option<LuaRegistryKey>>,
);

impl Service<Request<Body>> for NetServiceInner {
    type Response = Response<Body>;
    type Error = LuaError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let lua = self.0;
        if self.2.is_some() && is_ws_upgrade_request(&req) {
            // Websocket upgrade request + websocket handler exists,
            // we should now upgrade this connection to a websocket
            // and then call our handler with a new socket object
            let kopt = self.2.clone();
            let key = kopt.as_ref().as_ref().unwrap();
            let handler: LuaFunction = lua.registry_value(key).expect("Missing websocket handler");
            let (response, ws) = ws_upgrade(&mut req, None).expect("Failed to upgrade websocket");
            // This should be spawned as a registered task, otherwise
            // the scheduler may exit early and cancel this even though what
            // we want here is a long-running task that keeps the program alive
            let sched = lua
                .app_data_ref::<&TaskScheduler>()
                .expect("Missing task scheduler");
            let task = sched.register_background_task();
            task::spawn_local(async move {
                // Create our new full websocket object, then
                // schedule our handler to get called asap
                let ws = ws.await.map_err(LuaError::external)?;
                let sock = NetWebSocket::new(ws).into_lua_table(lua)?;
                let sched = lua
                    .app_data_ref::<&TaskScheduler>()
                    .expect("Missing task scheduler");
                let result = sched.schedule_blocking(
                    lua.create_thread(handler)?,
                    LuaMultiValue::from_vec(vec![LuaValue::Table(sock)]),
                );
                task.unregister(Ok(()));
                result
            });
            Box::pin(async move { Ok(response) })
        } else {
            // Got a normal http request or no websocket handler
            // exists, just call the http request handler
            let key = self.1.clone();
            let (parts, body) = req.into_parts();
            Box::pin(async move {
                // Convert request body into bytes, extract handler
                // function & lune message sender to use later
                let bytes = to_bytes(body).await.map_err(LuaError::external)?;
                let handler: LuaFunction = lua.registry_value(&key)?;
                // Create a readonly table for the request query params
                let query_params = TableBuilder::new(lua)?
                    .with_values(
                        parts
                            .uri
                            .query()
                            .unwrap_or_default()
                            .split('&')
                            .filter_map(|q| q.split_once('='))
                            .collect(),
                    )?
                    .build_readonly()?;
                // Do the same for headers
                let header_map = TableBuilder::new(lua)?
                    .with_values(
                        parts
                            .headers
                            .iter()
                            .map(|(name, value)| {
                                (name.to_string(), value.to_str().unwrap().to_string())
                            })
                            .collect(),
                    )?
                    .build_readonly()?;
                // Create a readonly table with request info to pass to the handler
                let request = TableBuilder::new(lua)?
                    .with_value("path", parts.uri.path())?
                    .with_value("query", query_params)?
                    .with_value("method", parts.method.as_str())?
                    .with_value("headers", header_map)?
                    .with_value("body", lua.create_string(&bytes)?)?
                    .build_readonly()?;
                // TODO: Make some kind of NetServeResponse type with a
                // FromLua implementation instead, this is a bit messy
                // and does not send errors to the scheduler properly
                match handler.call(request) {
                    // Plain strings from the handler are plaintext responses
                    Ok(LuaValue::String(s)) => Ok(Response::builder()
                        .status(200)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(s.as_bytes().to_vec()))
                        .unwrap()),
                    // Tables are more detailed responses with potential status, headers, body
                    Ok(LuaValue::Table(t)) => {
                        let status = t.get::<_, Option<u16>>("status")?.unwrap_or(200);
                        let mut resp = Response::builder().status(status);

                        if let Some(headers) = t.get::<_, Option<LuaTable>>("headers")? {
                            for pair in headers.pairs::<String, LuaString>() {
                                let (h, v) = pair?;
                                resp = resp.header(&h, v.as_bytes());
                            }
                        }

                        let body = t
                            .get::<_, Option<LuaString>>("body")?
                            .map(|b| Body::from(b.as_bytes().to_vec()))
                            .unwrap_or_else(Body::empty);

                        Ok(resp.body(body).unwrap())
                    }
                    // If the handler returns an error, generate a 5xx response
                    Err(_) => {
                        // TODO: Send above error to task scheduler so that it can emit properly
                        Ok(Response::builder()
                            .status(500)
                            .body(Body::from("Internal Server Error"))
                            .unwrap())
                    }
                    // If the handler returns a value that is of an invalid type,
                    // this should also be an error, so generate a 5xx response
                    Ok(_) => {
                        // TODO: Implement the type in the above todo
                        Ok(Response::builder()
                            .status(500)
                            .body(Body::from("Internal Server Error"))
                            .unwrap())
                    }
                }
            })
        }
    }
}

pub struct NetService(
    &'static Lua,
    Arc<LuaRegistryKey>,
    Arc<Option<LuaRegistryKey>>,
);

impl NetService {
    pub fn new(
        lua: &'static Lua,
        callback_http: LuaRegistryKey,
        callback_websocket: Option<LuaRegistryKey>,
    ) -> Self {
        Self(lua, Arc::new(callback_http), Arc::new(callback_websocket))
    }
}

impl Service<&AddrStream> for NetService {
    type Response = NetServiceInner;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: &AddrStream) -> Self::Future {
        let lua = self.0;
        let key1 = self.1.clone();
        let key2 = self.2.clone();
        Box::pin(async move { Ok(NetServiceInner(lua, key1, key2)) })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NetLocalExec;

impl<F> hyper::rt::Executor<F> for NetLocalExec
where
    F: std::future::Future + 'static, // not requiring `Send`
{
    fn execute(&self, fut: F) {
        task::spawn_local(fut);
    }
}
