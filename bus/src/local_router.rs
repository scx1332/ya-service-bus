use crate::{
    error::Error,
    remote_router::{RemoteRouter, UpdateService},
    Handle, RpcEnvelope, RpcHandler, RpcMessage, RpcRawCall,
};
use actix::{Actor, Message, Recipient, SystemService};
use futures::{
    compat::Future01CompatExt,
    future::{ready, Either},
    Future,
};
use futures_01::future::{Either as Either01, Future as Future01};
use std::{
    any::Any,
    sync::{Arc, Mutex},
};
use ya_sb_util::PrefixLookupBag;

mod into_actix;

trait RawEndpoint: Any {
    fn send(&self, msg: RpcRawCall) -> Box<dyn Future01<Item = Vec<u8>, Error = Error>>;

    fn recipient(&self) -> &dyn Any;
}

impl<T: RpcMessage> RawEndpoint for Recipient<RpcEnvelope<T>> {
    fn send(&self, msg: RpcRawCall) -> Box<dyn Future01<Item = Vec<u8>, Error = Error>> {
        let body: T = rmp_serde::decode::from_read(msg.body.as_slice()).unwrap();
        Box::new(
            Recipient::send(self, RpcEnvelope::with_caller(&msg.caller, body))
                .map_err(|e| e.into())
                .and_then(|r| rmp_serde::to_vec(&r).map_err(Error::from)),
        )
    }

    fn recipient(&self) -> &dyn Any {
        self
    }
}

impl RawEndpoint for Recipient<RpcRawCall> {
    fn send(&self, msg: RpcRawCall) -> Box<dyn Future01<Item = Vec<u8>, Error = Error>> {
        Box::new(
            Recipient::<RpcRawCall>::send(self, msg)
                .map_err(Error::from)
                .flatten(),
        )
    }

    fn recipient(&self) -> &dyn Any {
        self
    }
}

struct Slot {
    inner: Box<dyn RawEndpoint + Send + 'static>,
}

impl Slot {
    fn from_handler<T: RpcMessage, H: RpcHandler<T> + 'static>(handler: H) -> Self {
        Slot {
            inner: Box::new(
                into_actix::RpcHandlerWrapper::new(handler)
                    .start()
                    .recipient(),
            ),
        }
    }

    fn from_raw(r: Recipient<RpcRawCall>) -> Self {
        Slot { inner: Box::new(r) }
    }

    fn from_actor<T: RpcMessage>(r: Recipient<RpcEnvelope<T>>) -> Self {
        Slot { inner: Box::new(r) }
    }

    fn recipient<T: RpcMessage>(&mut self) -> Option<actix::Recipient<RpcEnvelope<T>>>
    where
        <RpcEnvelope<T> as Message>::Result: Sync + Send + 'static,
    {
        if let Some(r) = self
            .inner
            .recipient()
            .downcast_ref::<actix::Recipient<RpcEnvelope<T>>>()
        {
            Some(r.clone())
        } else {
            None
        }
    }

    fn send(&self, msg: RpcRawCall) -> impl Future01<Item = Vec<u8>, Error = Error> {
        self.inner.send(msg)
    }
}

pub struct Router {
    handlers: PrefixLookupBag<Slot>,
}

impl Router {
    fn new() -> Self {
        Router {
            handlers: PrefixLookupBag::default(),
        }
    }

    pub fn bind<T: RpcMessage>(
        &mut self,
        addr: &str,
        endpoint: impl RpcHandler<T> + 'static,
    ) -> Handle {
        let slot = Slot::from_handler(endpoint);
        let addr = format!("{}/{}", addr, T::ID);
        let _ = self.handlers.insert(addr.clone(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr.into()));
        Handle { _inner: () }
    }

    pub fn bind_actor<T: RpcMessage>(&mut self, addr: &str, endpoint: Recipient<RpcEnvelope<T>>) {
        let slot = Slot::from_actor(endpoint);
        let addr = format!("{}/{}", addr, T::ID);
        let _ = self.handlers.insert(addr.clone(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr));
    }

    pub fn bind_raw(&mut self, addr: &str, endpoint: Recipient<RpcRawCall>) -> Handle {
        let slot = Slot::from_raw(endpoint);
        let _ = self.handlers.insert(addr.to_string(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr.into()));
        Handle { _inner: () }
    }

    pub fn forward<T: RpcMessage>(
        &mut self,
        addr: &str,
        msg: T,
    ) -> impl Future01<Item = Result<T::Item, T::Error>, Error = Error> {
        let caller = "local";
        let addr = format!("{}/{}", addr, T::ID);
        if let Some(slot) = self.handlers.get_mut(&addr) {
            Either01::A(if let Some(h) = slot.recipient() {
                Either01::A(h.send(RpcEnvelope::local(msg)).map_err(Error::from))
            } else {
                let body = rmp_serde::to_vec(&msg).unwrap();
                Either01::B(
                    slot.send(RpcRawCall {
                        caller: caller.into(),
                        addr,
                        body,
                    })
                    .and_then(|b| Ok(rmp_serde::from_read_ref(&b)?)),
                )
            })
        } else {
            let body = rmp_serde::to_vec(&msg).unwrap();
            Either01::B(
                RemoteRouter::from_registry()
                    .send(RpcRawCall {
                        caller: caller.into(),
                        addr,
                        body,
                    })
                    .flatten()
                    .and_then(|b| Ok(rmp_serde::from_read_ref(&b)?)),
            )
        }
    }

    pub fn forward_bytes(
        &mut self,
        addr: &str,
        from: &str,
        msg: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> {
        if let Some(slot) = self.handlers.get_mut(addr) {
            Either::Left(
                slot.send(RpcRawCall {
                    caller: from.into(),
                    addr: addr.into(),
                    body: msg,
                })
                .compat(),
            )
        } else {
            Either::Right(
                RemoteRouter::from_registry()
                    .send(RpcRawCall {
                        caller: from.into(),
                        addr: addr.into(),
                        body: msg,
                    })
                    .flatten()
                    .compat(),
            )
        }
    }

    pub fn forward_bytes_local(
        &mut self,
        addr: &str,
        from: &str,
        msg: &[u8],
    ) -> impl Future<Output = Result<Vec<u8>, Error>> {
        if let Some(slot) = self.handlers.get_mut(addr) {
            Either::Left(
                slot.send(RpcRawCall {
                    caller: from.into(),
                    addr: addr.into(),
                    body: msg.into(),
                })
                .compat(),
            )
        } else {
            log::warn!("no endpoint: {}", addr);
            Either::Right(ready(Err(Error::NoEndpoint)))
        }
    }
}

lazy_static::lazy_static! {
static ref ROUTER: Arc<Mutex<Router>> = Arc::new(Mutex::new(Router::new()));
}

pub fn router() -> Arc<Mutex<Router>> {
    ROUTER.clone()
}
