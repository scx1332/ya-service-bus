use actix::{Actor, Arbiter, Message, Recipient, SystemService};
use futures::{prelude::*, FutureExt, StreamExt};
use std::any::Any;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ya_sb_util::futures::IntoFlatten;
use ya_sb_util::PrefixLookupBag;

use crate::{
    remote_router::{RemoteRouter, UpdateService},
    Error, Handle, ResponseChunk, RpcEnvelope, RpcHandler, RpcMessage, RpcRawCall,
    RpcRawStreamCall, RpcStreamCall, RpcStreamHandler, RpcStreamMessage,
};
use futures::channel::mpsc;

mod into_actix;

struct DualRawEndpoint {
    rpc: Recipient<RpcRawCall>,
    stream: Recipient<RpcRawStreamCall>,
}

impl DualRawEndpoint {
    pub fn new(rpc: Recipient<RpcRawCall>, stream: Recipient<RpcRawStreamCall>) -> Self {
        DualRawEndpoint { rpc, stream }
    }
}

trait RawEndpoint: Any {
    fn send(&self, msg: RpcRawCall) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>>>>;

    fn call_stream(
        &self,
        msg: RpcRawCall,
    ) -> Pin<Box<dyn Stream<Item = Result<ResponseChunk, Error>>>>;

    fn recipient(&self) -> &dyn Any;
}

// Implementation for non-streaming service
impl<T: RpcMessage> RawEndpoint for Recipient<RpcEnvelope<T>> {
    fn send(&self, msg: RpcRawCall) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>>>> {
        let body: T =
            match crate::serialization::from_slice(msg.body.as_slice()).map_err(Error::from) {
                Ok(v) => v,
                Err(e) => return future::err(e).boxed_local(),
            };
        Box::pin(
            Recipient::send(self, RpcEnvelope::with_caller(&msg.caller, body))
                .map_err(|e| Error::from_addr(msg.addr, e))
                .and_then(|r| async move { crate::serialization::to_vec(&r).map_err(Error::from) }),
        )
    }

    fn call_stream(
        &self,
        msg: RpcRawCall,
    ) -> Pin<Box<dyn Stream<Item = Result<ResponseChunk, Error>>>> {
        let body: T =
            match crate::serialization::from_slice(msg.body.as_slice()).map_err(Error::from) {
                Ok(v) => v,
                Err(e) => return Box::pin(stream::once(async { Err::<ResponseChunk, Error>(e) })),
            };

        Box::pin(
            Recipient::send(self, RpcEnvelope::with_caller(&msg.caller, body))
                .map_err(|e| Error::from_addr(msg.addr, e))
                .and_then(|r| future::ready(crate::serialization::to_vec(&r).map_err(Error::from)))
                .map_ok(|v| ResponseChunk::Full(v))
                .into_stream(),
        )
    }

    fn recipient(&self) -> &dyn Any {
        self
    }
}

impl<T: RpcStreamMessage> RawEndpoint for Recipient<RpcStreamCall<T>> {
    fn send(&self, msg: RpcRawCall) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>>>> {
        Box::pin(future::err(Error::GsbBadRequest(format!(
            "non-streaming-request on streaming endpoint: {}",
            msg.addr
        ))))
    }

    fn call_stream(
        &self,
        msg: RpcRawCall,
    ) -> Pin<Box<dyn Stream<Item = Result<ResponseChunk, Error>>>> {
        let body: T = crate::serialization::from_slice(msg.body.as_slice()).unwrap();
        let (tx, rx) = futures::channel::mpsc::channel(16);
        let (txe, rxe) = futures::channel::oneshot::channel();

        let addr = msg.addr.clone();
        let call = RpcStreamCall {
            caller: msg.caller,
            addr: msg.addr,
            body,
            reply: tx,
        };
        let me = self.clone();
        Arbiter::current().spawn(async move {
            match me.send(call).await {
                Err(e) => {
                    let _ = txe.send(Err(Error::from_addr(addr, e)));
                }
                Ok(Err(e)) => {
                    let _ = txe.send(Err(e));
                }
                Ok(Ok(())) => (),
            };
        });

        let recv_stream = rx
            .then(|r| {
                future::ready(
                    crate::serialization::to_vec(&r)
                        .map_err(Error::from)
                        .map(|r| ResponseChunk::Part(r)),
                )
            })
            .chain(rxe.into_stream().filter_map(|v| future::ready(v.ok())));

        Box::pin(recv_stream)
    }

    fn recipient(&self) -> &dyn Any {
        self
    }
}

impl RawEndpoint for Recipient<RpcRawCall> {
    fn send(&self, msg: RpcRawCall) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>>>> {
        let addr = msg.addr.clone();
        Box::pin(
            Recipient::<RpcRawCall>::send(self, msg)
                .map_err(|e| Error::from_addr(addr, e))
                .then(|v| async { v? }),
        )
    }

    fn call_stream(
        &self,
        msg: RpcRawCall,
    ) -> Pin<Box<dyn Stream<Item = Result<ResponseChunk, Error>>>> {
        let addr = msg.addr.clone();
        Box::pin(
            Recipient::<RpcRawCall>::send(self, msg)
                .map_err(|e| Error::from_addr(addr, e))
                .flatten_fut()
                .and_then(|v| future::ok(ResponseChunk::Full(v)))
                .into_stream(),
        )
    }

    fn recipient(&self) -> &dyn Any {
        self
    }
}

impl RawEndpoint for Recipient<RpcRawStreamCall> {
    fn send(&self, msg: RpcRawCall) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>>>> {
        let (tx, rx) = futures::channel::mpsc::channel(1);
        // TODO: send error to caller
        Arbiter::current().spawn(
            self.send(RpcRawStreamCall {
                caller: msg.caller,
                addr: msg.addr,
                body: msg.body,
                reply: tx,
            })
            .flatten_fut()
            .map_err(|e| eprintln!("cell error={}", e))
            .then(|_v| future::ready(())),
        );
        async move {
            futures::pin_mut!(rx);
            match StreamExt::next(&mut rx).await {
                Some(Ok(ResponseChunk::Full(v))) => Ok(v),
                Some(Ok(ResponseChunk::Part(_))) => {
                    Err(Error::GsbBadRequest("partial response".into()))
                }
                Some(Err(e)) => Err(e),
                None => Err(Error::GsbBadRequest("unexpected EOS".into())),
            }
        }
        .boxed_local()
    }

    fn call_stream(
        &self,
        msg: RpcRawCall,
    ) -> Pin<Box<dyn Stream<Item = Result<ResponseChunk, Error>>>> {
        let (tx, rx) = futures::channel::mpsc::channel(16);
        // TODO: send error to caller
        Arbiter::current().spawn(
            self.send(RpcRawStreamCall {
                caller: msg.caller,
                addr: msg.addr,
                body: msg.body,
                reply: tx,
            })
            .flatten_fut()
            .map_err(|e| eprintln!("cell error={}", e))
            .then(|_| future::ready(())),
        );
        Box::pin(rx)
    }

    fn recipient(&self) -> &dyn Any {
        self
    }
}

impl RawEndpoint for DualRawEndpoint {
    fn send(&self, msg: RpcRawCall) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>>>> {
        RawEndpoint::send(&self.rpc, msg)
    }

    fn call_stream(
        &self,
        msg: RpcRawCall,
    ) -> Pin<Box<dyn Stream<Item = Result<ResponseChunk, Error>>>> {
        RawEndpoint::call_stream(&self.stream, msg)
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

    fn from_stream_handler<T: RpcStreamMessage, H: RpcStreamHandler<T> + 'static>(
        handler: H,
    ) -> Self {
        Slot {
            inner: Box::new(
                into_actix::RpcStreamHandlerWrapper::new(handler)
                    .start()
                    .recipient(),
            ),
        }
    }

    #[allow(unused)]
    fn from_raw(r: Recipient<RpcRawCall>) -> Self {
        Slot { inner: Box::new(r) }
    }

    fn from_raw_dual(r: DualRawEndpoint) -> Self {
        Slot { inner: Box::new(r) }
    }

    fn from_actor<T: RpcMessage>(r: Recipient<RpcEnvelope<T>>) -> Self {
        Slot { inner: Box::new(r) }
    }

    fn from_stream_actor<T: RpcStreamMessage>(r: Recipient<RpcStreamCall<T>>) -> Self {
        Slot { inner: Box::new(r) }
    }

    fn recipient<T: RpcMessage>(&mut self) -> Option<actix::Recipient<RpcEnvelope<T>>>
    where
        <RpcEnvelope<T> as Message>::Result: Sync + Send + 'static,
    {
        self.inner
            .recipient()
            .downcast_ref::<actix::Recipient<RpcEnvelope<T>>>()
            .cloned()
    }

    fn stream_recipient<T: RpcStreamMessage>(&self) -> Option<actix::Recipient<RpcStreamCall<T>>> {
        self.inner
            .recipient()
            .downcast_ref::<actix::Recipient<RpcStreamCall<T>>>()
            .cloned()
    }

    fn raw_stream_recipient(&self) -> Option<actix::Recipient<RpcRawStreamCall>> {
        if let Some(e) = self.inner.recipient().downcast_ref::<DualRawEndpoint>() {
            Some(e.stream.clone())
        } else {
            self.inner
                .recipient()
                .downcast_ref::<actix::Recipient<RpcRawStreamCall>>()
                .cloned()
        }
    }

    fn send(&self, msg: RpcRawCall) -> impl Future<Output = Result<Vec<u8>, Error>> + Unpin {
        self.inner.send(msg)
    }

    fn send_streaming(&self, msg: RpcRawCall) -> impl Stream<Item = Result<ResponseChunk, Error>> {
        self.inner.call_stream(msg)
    }

    fn streaming_forward<T: RpcStreamMessage>(
        &self,
        caller: String,
        addr: String,
        body: T,
    ) -> impl Stream<Item = Result<Result<T::Item, T::Error>, Error>> {
        let no_reply = false;

        if let Some(h) = self.stream_recipient() {
            let (reply, rx) = futures::channel::mpsc::channel(16);
            let call = RpcStreamCall {
                caller,
                addr,
                body,
                reply,
            };

            Arbiter::current().spawn(async move {
                h.send(call)
                    .await
                    .unwrap_or_else(|e| Ok(log::error!("streaming forward error: {}", e)))
                    .unwrap_or_else(|e| log::error!("streaming forward error: {}", e));
            });
            rx.map(|v| Ok(v)).boxed_local().left_stream()
        } else if let Some(h) = self.raw_stream_recipient() {
            (move || {
                let (reply, rx) = futures::channel::mpsc::channel(16);
                let body = match crate::serialization::to_vec(&body) {
                    Ok(body) => body,
                    Err(e) => return stream::once(future::err(Error::from(e))).right_stream(),
                };
                let call = RpcRawStreamCall {
                    caller,
                    addr,
                    body,
                    reply,
                };

                Arbiter::current().spawn(async move {
                    h.send(call)
                        .await
                        .unwrap_or_else(|e| Ok(log::error!("streaming raw forward error: {}", e)))
                        .unwrap_or_else(|e| log::error!("streaming raw forward error: {}", e));
                });
                rx.filter(|s| future::ready(s.as_ref().map(|s| !s.is_eos()).unwrap_or(true)))
                    .map(|chunk_result| {
                        (move || -> Result<Result<T::Item, T::Error>, Error> {
                            let chunk = match chunk_result {
                                Ok(ResponseChunk::Part(chunk)) => chunk,
                                Ok(ResponseChunk::Full(chunk)) => chunk,
                                Err(e) => return Err(e),
                            };
                            Ok(crate::serialization::from_slice(&chunk)?)
                        })()
                    })
                    .left_stream()
            })()
            .boxed_local()
            .right_stream()
        } else {
            (move || {
                let body = match crate::serialization::to_vec(&body) {
                    Ok(body) => body,
                    Err(e) => return stream::once(future::err(Error::from(e))).right_stream(),
                };
                self.send_streaming(RpcRawCall {
                    caller,
                    addr,
                    body,
                    no_reply,
                })
                .filter(|s| future::ready(s.as_ref().map(|s| !s.is_eos()).unwrap_or(true)))
                .map(|chunk_result| {
                    (move || -> Result<Result<T::Item, T::Error>, Error> {
                        let chunk = match chunk_result {
                            Ok(ResponseChunk::Part(chunk)) => chunk,
                            Ok(ResponseChunk::Full(chunk)) => chunk,
                            Err(e) => return Err(e),
                        };
                        Ok(crate::serialization::from_slice(&chunk)?)
                    })()
                })
                .left_stream()
            })()
            .boxed_local()
            .right_stream()
        }
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
        log::debug!("binding {}", addr);
        let _ = self.handlers.insert(addr.clone(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr));
        Handle { _inner: () }
    }

    pub fn unbind(&mut self, addr: &str) -> impl Future<Output = Result<bool, Error>> + Unpin {
        let pattern = match addr.ends_with('/') {
            true => addr.to_string(),
            false => format!("{}/", addr),
        };
        let addrs = self
            .handlers
            .keys()
            .filter(|a| a.starts_with(&pattern))
            .cloned()
            .collect::<Vec<String>>();

        addrs.iter().for_each(|addr| {
            log::debug!("unbinding {}", addr);
            self.handlers.remove(addr);
        });

        Box::pin(async move {
            let router = RemoteRouter::from_registry();
            let success = !addrs.is_empty();
            for addr in addrs {
                router
                    .send(UpdateService::Remove(addr.clone()))
                    .await
                    .map_err(|e| Error::from_addr(addr, e))?;
            }
            Ok(success)
        })
    }

    pub fn bind_stream<T: RpcStreamMessage>(
        &mut self,
        addr: &str,
        endpoint: impl RpcStreamHandler<T> + Unpin + 'static,
    ) -> Handle {
        let slot = Slot::from_stream_handler(endpoint);
        let addr = format!("{}/{}", addr, T::ID);
        log::debug!("binding stream {}", addr);
        let _ = self.handlers.insert(addr.clone(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr));
        Handle { _inner: () }
    }

    pub fn bind_stream_actor<T: RpcStreamMessage>(
        &mut self,
        addr: &str,
        endpoint: Recipient<RpcStreamCall<T>>,
    ) {
        let slot = Slot::from_stream_actor(endpoint);
        let addr = format!("{}/{}", addr, T::ID);
        log::debug!("binding stream actor {}", addr);
        let _ = self.handlers.insert(addr.clone(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr));
    }

    pub fn bind_actor<T: RpcMessage>(&mut self, addr: &str, endpoint: Recipient<RpcEnvelope<T>>) {
        let slot = Slot::from_actor(endpoint);
        let addr = format!("{}/{}", addr, T::ID);
        log::debug!("binding actor {}", addr);
        let _ = self.handlers.insert(addr.clone(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr));
    }

    #[allow(unused)]
    pub fn bind_raw(&mut self, addr: &str, endpoint: Recipient<RpcRawCall>) -> Handle {
        let slot = Slot::from_raw(endpoint);
        log::debug!("binding raw {}", addr);
        let _ = self.handlers.insert(addr.to_string(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr.into()));
        Handle { _inner: () }
    }

    pub fn bind_raw_dual(
        &mut self,
        addr: &str,
        rpc: Recipient<RpcRawCall>,
        stream: Recipient<RpcRawStreamCall>,
    ) -> Handle {
        let slot = Slot::from_raw_dual(DualRawEndpoint::new(rpc, stream));
        log::debug!("binding raw + stream {}", addr);
        let _ = self.handlers.insert(addr.to_string(), slot);
        RemoteRouter::from_registry().do_send(UpdateService::Add(addr.into()));
        Handle { _inner: () }
    }

    pub fn forward<T: RpcMessage + Unpin>(
        &mut self,
        addr: &str,
        msg: RpcEnvelope<T>,
    ) -> impl Future<Output = Result<Result<T::Item, T::Error>, Error>> {
        let addr = format!("{}/{}", addr, T::ID);
        if let Some(slot) = self.handlers.get_mut(&addr) {
            (if let Some(h) = slot.recipient() {
                h.send(msg)
                    .map_err(|e| Error::from_addr(addr, e))
                    .left_future()
            } else {
                slot.send(RpcRawCall::from_envelope_addr(msg, addr, false))
                    .then(|b| {
                        future::ready(match b {
                            Ok(b) => {
                                if b.is_empty() {
                                    Err(Error::GsbFailure(
                                        "empty response from remote service".to_string(),
                                    ))
                                } else {
                                    crate::serialization::from_slice(&b).map_err(From::from)
                                }
                            }
                            Err(e) => Err(e),
                        })
                    })
                    .right_future()
            })
            .left_future()
        } else {
            RemoteRouter::from_registry()
                .send(RpcRawCall::from_envelope_addr(msg, addr.clone(), false))
                .then(|v| {
                    future::ready(match v {
                        Ok(v) => v,
                        Err(e) => Err(Error::from_addr(addr, e)),
                    })
                })
                .then(|b| {
                    future::ready(match b {
                        Ok(b) => {
                            if b.is_empty() {
                                Err(Error::GsbFailure(
                                    "empty response from remote service".to_string(),
                                ))
                            } else {
                                crate::serialization::from_slice(&b).map_err(From::from)
                            }
                        }
                        Err(e) => Err(e),
                    })
                })
                .right_future()
        }
    }

    pub fn push<T: RpcMessage + Unpin>(
        &mut self,
        addr: &str,
        msg: RpcEnvelope<T>,
    ) -> impl Future<Output = Result<(), Error>> {
        let addr = format!("{}/{}", addr, T::ID);
        if let Some(slot) = self.handlers.get_mut(&addr) {
            if let Some(h) = slot.recipient() {
                h.send(msg)
                    .then(|v| {
                        future::ready(match v {
                            Ok(_) => Ok(()),
                            Err(e) => Err(Error::from_addr(addr, e)),
                        })
                    })
                    .left_future()
            } else {
                slot.send(RpcRawCall::from_envelope_addr(msg, addr.clone(), true))
                    .then(|v| future::ready(v.map(|_| ())))
                    .right_future()
            }
            .left_future()
        } else {
            RemoteRouter::from_registry()
                .send(RpcRawCall::from_envelope_addr(msg, addr.clone(), true))
                .then(|v| {
                    future::ready(match v {
                        Ok(_) => Ok(()),
                        Err(e) => Err(Error::from_addr(addr, e)),
                    })
                })
                .right_future()
        }
    }

    pub fn streaming_forward<T: RpcStreamMessage>(
        &mut self,
        addr: &str,
        // TODO: add `from: &str` as in `forward_bytes` below
        msg: T,
    ) -> impl Stream<Item = Result<Result<T::Item, T::Error>, Error>> {
        let caller = "local".to_string();
        let addr = format!("{}/{}", addr, T::ID);
        if let Some(slot) = self.handlers.get_mut(&addr) {
            slot.streaming_forward(caller, addr, msg).left_stream()
        } else {
            //use futures::StreamExt;
            log::trace!("call remote (stream) {}", addr);
            let body = crate::serialization::to_vec(&msg).unwrap();
            let (reply, tx) = futures::channel::mpsc::channel(16);
            let call = RpcRawStreamCall {
                caller,
                addr,
                body,
                reply,
            };
            let _ = Arbiter::current().spawn(async move {
                let v = RemoteRouter::from_registry().send(call).await;
                log::trace!("call result={:?}", v);
            });

            tx.filter(|s| future::ready(s.as_ref().map(|s| !s.is_eos()).unwrap_or(true)))
                .map(|b| {
                    let body = b?.into_bytes();
                    Ok(crate::serialization::from_slice(&body)?)
                })
                .right_stream()
        }
    }

    pub fn forward_bytes(
        &mut self,
        addr: &str,
        caller: &str,
        msg: Vec<u8>,
        no_reply: bool,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> {
        let addr = addr.to_string();
        if let Some(slot) = self.handlers.get_mut(&addr) {
            slot.send(RpcRawCall {
                caller: caller.into(),
                addr: addr.clone(),
                body: msg,
                no_reply,
            })
            .left_future()
        } else {
            RemoteRouter::from_registry()
                .send(RpcRawCall {
                    caller: caller.into(),
                    addr: addr.clone(),
                    body: msg,
                    no_reply,
                })
                .then(|v| match v {
                    Ok(r) => future::ready(r),
                    Err(e) => future::err(Error::from_addr(addr, e)),
                })
                .right_future()
        }
    }

    pub fn streaming_forward_bytes(
        &mut self,
        addr: &str,
        caller: &str,
        msg: Vec<u8>,
    ) -> impl Stream<Item = Result<ResponseChunk, Error>> {
        if let Some(slot) = self.handlers.get_mut(addr) {
            slot.send_streaming(RpcRawCall {
                caller: caller.into(),
                addr: addr.into(),
                body: msg,
                no_reply: false,
            })
            .left_stream()
        } else {
            let (tx, rx) = mpsc::channel(16);
            let call = RpcRawStreamCall {
                caller: caller.into(),
                addr: addr.into(),
                body: msg,
                reply: tx,
            };
            async move {
                match RemoteRouter::from_registry().send(call).await {
                    Ok(_) => rx.boxed_local(),
                    Err(e) => futures::stream::once(future::err(e.into())).boxed_local(),
                }
            }
            .flatten_stream()
            .right_stream()
        }
    }

    pub fn forward_bytes_local(
        &mut self,
        addr: &str,
        caller: &str,
        msg: &[u8],
        no_reply: bool,
    ) -> impl Stream<Item = Result<ResponseChunk, Error>> {
        let addr = addr.to_string();
        if let Some(slot) = self.handlers.get_mut(&addr) {
            let msg = RpcRawCall {
                caller: caller.into(),
                addr,
                body: msg.into(),
                no_reply,
            };

            if no_reply {
                let fut = slot.send(msg);
                futures::stream::once(async move { fut.await.map(ResponseChunk::Full) })
                    .boxed_local()
            } else {
                slot.send_streaming(msg).boxed_local()
            }
        } else {
            log::warn!("no endpoint: {}", addr);
            futures::stream::once(async { Err(Error::NoEndpoint(addr)) }).boxed_local()
        }
    }
}

lazy_static::lazy_static! {
static ref ROUTER: Arc<Mutex<Router>> = Arc::new(Mutex::new(Router::new()));
}

pub fn router() -> Arc<Mutex<Router>> {
    (*ROUTER).clone()
}
