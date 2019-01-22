/*
 * Copyright 2019 OysterPack Inc.
 *
 *    Licensed under the Apache License, Version 2.0 (the "License");
 *    you may not use this file except in compliance with the License.
 *    You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing, software
 *    distributed under the License is distributed on an "AS IS" BASIS,
 *    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *    See the License for the specific language governing permissions and
 *    limitations under the License.
 */

//! Provides support for request/reply messaging via async futures based channels.

use crate::concurrent::messaging::{errors::ChannelSendError, MessageId};
use futures::{channel, sink::SinkExt};
use oysterpack_uid::macros::ulid;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// Implements a request/reply messaging pattern
/// - Each ReqRep is assigned a unique ReqRepId
#[derive(Debug, Clone)]
pub struct ReqRep<Req, Rep>
where
    Req: Debug + Send + 'static,
    Rep: Debug + Send + 'static,
{
    request_sender: channel::mpsc::Sender<ReqRepMessage<Req, Rep>>,
    reqrep_id: ReqRepId,
}

impl<Req, Rep> ReqRep<Req, Rep>
where
    Req: Debug + Send + 'static,
    Rep: Debug + Send + 'static,
{
    /// Send the request
    /// - each request message is assigned a MessageId, which is returned within the ReplyReceiver
    pub async fn send(&mut self, req: Req) -> Result<ReplyReceiver<Rep>, ChannelSendError> {
        let (rep_sender, rep_receiver) = channel::oneshot::channel::<Rep>();
        let msg_id = MessageId::generate();
        let msg = ReqRepMessage {
            req: Some(req),
            rep_sender,
            msg_id,
            reqrep_id: self.reqrep_id,
        };
        await!(self.request_sender.send(msg))?;
        Ok(ReplyReceiver {
            msg_id,
            receiver: rep_receiver,
        })
    }

    /// constructor
    pub fn new(
        reqrep_id: ReqRepId,
        chan_buf_size: usize,
    ) -> (
        ReqRep<Req, Rep>,
        channel::mpsc::Receiver<ReqRepMessage<Req, Rep>>,
    ) {
        let (request_sender, request_receiver) = channel::mpsc::channel(chan_buf_size);
        (
            ReqRep {
                reqrep_id,
                request_sender,
            },
            request_receiver,
        )
    }
}

/// Message used for request/reply patterns.
#[derive(Debug)]
pub struct ReqRepMessage<Req, Rep>
where
    Req: Debug + Send + 'static,
    Rep: Debug + Send + 'static,
{
    reqrep_id: ReqRepId,
    msg_id: MessageId,
    req: Option<Req>,
    rep_sender: channel::oneshot::Sender<Rep>,
}

impl<Req, Rep> ReqRepMessage<Req, Rep>
where
    Req: Debug + Send + 'static,
    Rep: Debug + Send + 'static,
{
    /// Take the request, i.e., which transfers ownership
    ///
    /// ## Notes
    /// - this can only be called once - once the request message is taken, None is always returned
    pub fn take_request(&mut self) -> Option<Req> {
        self.req.take()
    }

    /// Send the reply
    pub fn reply(self, rep: Rep) -> Result<(), ChannelSendError> {
        self.rep_sender
            .send(rep)
            .map_err(|_| ChannelSendError::Disconnected)
    }

    /// Returns the ReqRepId
    pub fn reqrep_id(&self) -> ReqRepId {
        self.reqrep_id
    }

    /// Returns the request MessageId
    pub fn message_id(&self) -> MessageId {
        self.msg_id
    }
}

/// Each request/reply API is uniquely identified by an ID.
///
/// ## Use Cases
/// 1. Used for tracking purposes
/// 2. Used to map to a ReqRep message processor
#[ulid]
pub struct ReqRepId(pub u128);

/// Reply Receiver
#[derive(Debug)]
pub struct ReplyReceiver<Rep>
where
    Rep: Debug + Send + 'static,
{
    msg_id: MessageId,
    receiver: channel::oneshot::Receiver<Rep>,
}

impl<Rep> ReplyReceiver<Rep>
where
    Rep: Debug + Send + 'static,
{
    /// Request message id
    pub fn message_id(&self) -> MessageId {
        self.msg_id
    }

    /// Receive the reply
    ///
    /// ## Notes
    /// If the MessageId is required for tracking purposes, then it must be retrieved before
    /// invoking recv because this method consumes the object.
    pub async fn recv(self) -> Result<Rep, channel::oneshot::Canceled> {
        await!(self.receiver)
    }

    /// Closes the receiver channel
    pub fn close(self) {
        let mut this = self;
        this.receiver.close()
    }
}

#[allow(warnings)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrent::execution::EXECUTORS;
    use crate::log_config;
    use futures::{
        channel::oneshot,
        stream::StreamExt,
        task::{Spawn, SpawnExt},
    };
    use oysterpack_log::*;
    use std::thread;

    #[test]
    fn req_rep() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);
        const REQREP_ID: ReqRepId = ReqRepId(1871557337320005579010710867531265404);
        let executors = EXECUTORS.lock().unwrap();
        let mut executor = executors.global_executor();
        let (mut req_rep, req_receiver) = ReqRep::<usize, usize>::new(REQREP_ID, 1);
        let server = async move {
            let mut req_receiver = req_receiver;
            while let Some(mut msg) = await!(req_receiver.next()) {
                assert_eq!(msg.reqrep_id(), REQREP_ID);
                info!(
                    "Received request: ReqRepId({}) MessageId({})",
                    msg.reqrep_id(),
                    msg.message_id()
                );
                let n = msg.take_request().unwrap();
                if let Err(err) = msg.reply(n + 1) {
                    warn!("{}", err);
                }
            }
            info!("message listener has exited");
        };
        executor.spawn(server);
        let task = async {
            let rep_receiver = await!(req_rep.send(1)).unwrap();
            info!("request MessageId: {}", rep_receiver.message_id());
            await!(rep_receiver.recv()).unwrap()
        };
        let n = executor.run(task);
        info!("n = {}", n);
        assert_eq!(n, 2);
    }

    #[test]
    fn req_rep_with_disconnected_receiver() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);
        const REQREP_ID: ReqRepId = ReqRepId(1871557337320005579010710867531265404);
        let executors = EXECUTORS.lock().unwrap();
        let mut executor = executors.global_executor();
        let (mut req_rep, req_receiver) = ReqRep::<usize, usize>::new(REQREP_ID, 1);
        let server = async move {
            let mut req_receiver = req_receiver;
            if let Some(mut msg) = await!(req_receiver.next()) {
                let n = msg.take_request().unwrap();
                info!("going to sleep ...");
                thread::sleep_ms(10);
                info!("... awoke");
                if let Err(err) = msg.reply(n + 1) {
                    warn!("{}", err);
                } else {
                    panic!("Should have failed to send reply because the Receiver has been closed");
                }
            }
            info!("message listener has exited");
        };
        let task_handle = executor.spawn_with_handle(server).unwrap();
        let task = async {
            let mut rep_receiver = await!(req_rep.send(1)).unwrap();
            rep_receiver.close();
        };
        executor.run(task);
        executor.run(task_handle);
    }
}
