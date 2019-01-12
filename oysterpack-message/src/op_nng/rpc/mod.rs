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

//! Provides support for a request/reply RPC-like services.

use log::{error, info};
use oysterpack_uid::ULID;
use std::{
    num::NonZeroUsize,
    panic::{RefUnwindSafe, UnwindSafe},
    thread,
};

pub mod client;
pub mod server;

/// MessageProcessor factory
pub trait MessageProcessorFactory<T, Req, Rep>: Send + Sync + 'static
where
    Req: Send + 'static,
    Rep: Send + 'static,
    T: MessageProcessor<Req, Rep>,
{
    /// returns a new MessageProcessor instance
    fn new(&self) -> T;
}

/// Message handler that implements a request/reply protocol pattern
pub trait MessageProcessor<Req, Rep>:
    Send + Sync + Sized + RefUnwindSafe + UnwindSafe + 'static
where
    Req: Send + 'static,
    Rep: Send + 'static,
{
    /// processes the request message and returns a reply message
    fn process(&mut self, req: Req) -> Rep;

    /// Binds the MessageHandler to the MessageService channels.
    ///
    /// The MessageHandler will run in a background thread until one of the following events occurs:
    /// 1. the request sender channel is dropped, i.e., disconnected and after all messages on the request receiver
    ///    channel have been processed.
    /// 2. the reply receiver channel is dropped, i.e., disconnected
    fn bind(self, channels: MessageServiceChannel<Req, Rep>, thread_config: Option<ThreadConfig>) {
        let builder =
            thread_config.map_or_else(thread::Builder::new, |config| match config.stack_size {
                None => thread::Builder::new().name(config.name),
                Some(stack_size) => thread::Builder::new()
                    .name(config.name)
                    .stack_size(stack_size),
            });

        builder
            .spawn(move || {
                let t = thread::current();
                info!(
                    "MessageHandler started: {} : {:?}",
                    t.name().map_or("", |name| name),
                    t.id()
                );
                let mut handler = self;
                for msg in channels.request_channel {
                    let reply = handler.process(msg);
                    if let Err(err) = channels.reply_channel.send(reply) {
                        // means the channel has been disconnected
                        error!("Failed to send reply message: {}", err);
                    }
                }
                info!(
                    "MessageHandler stopped: {} : {:?}",
                    t.name().map_or("", |name| name),
                    t.id()
                )
            })
            .unwrap();
    }

    // TODO: test
    /// Binds the message handler to the receiver channel, which is used to receive request messages.
    /// The reply callback is invoked to handle the reply message.
    fn bind_with_reply_callback<F>(
        self,
        receiver: crossbeam::channel::Receiver<Req>,
        thread_config: Option<ThreadConfig>,
        reply_callback: F,
    ) where
        F: FnMut(Rep) + Send + 'static,
    {
        let builder =
            thread_config.map_or_else(thread::Builder::new, |config| match config.stack_size {
                None => thread::Builder::new().name(config.name),
                Some(stack_size) => thread::Builder::new()
                    .name(config.name)
                    .stack_size(stack_size),
            });

        let mut handle_reply = reply_callback;

        builder
            .spawn(move || {
                let t = thread::current();
                info!(
                    "MessageHandler started: {} : {:?}",
                    t.name().map_or("", |name| name),
                    t.id()
                );
                let mut handler = self;
                for msg in receiver {
                    let reply = handler.process(msg);
                    handle_reply(reply);
                }
                info!(
                    "MessageHandler stopped: {} : {:?}",
                    t.name().map_or("", |name| name),
                    t.id()
                )
            })
            .unwrap();
    }
}

/// constructs a pair of request/reply channels
/// <pre>
///     client ---req--> service
///     client <--rep--- service
/// </pre>
///
/// Each channel is bounded using the specified capacity. This means sending on the channel will block
/// when the channel is full.
pub fn channels<Req, Rep>(
    req_cap: usize,
    rep_cap: usize,
) -> (
    MessageClientChannel<Req, Rep>,
    MessageServiceChannel<Req, Rep>,
)
where
    Req: Send,
    Rep: Send,
{
    let (request_sender, request_receiver) = crossbeam::channel::bounded(req_cap);
    let (reply_sender, reply_receiver) = crossbeam::channel::bounded(rep_cap);
    (
        MessageClientChannel {
            request_channel: request_sender,
            reply_channel: reply_receiver,
        },
        MessageServiceChannel {
            request_channel: request_receiver,
            reply_channel: reply_sender,
        },
    )
}

/// Message service channel
#[derive(Debug, Clone)]
pub struct MessageServiceChannel<Req, Rep>
where
    Req: Send,
    Rep: Send,
{
    request_channel: crossbeam::Receiver<Req>,
    reply_channel: crossbeam::Sender<Rep>,
}

impl<Req, Rep> MessageServiceChannel<Req, Rep>
where
    Req: Send,
    Rep: Send,
{
    /// used by the service to receive messages
    pub fn receiver(&self) -> &crossbeam::Receiver<Req> {
        &self.request_channel
    }

    /// used by the service to send replies
    pub fn sender(&self) -> &crossbeam::Sender<Rep> {
        &self.reply_channel
    }
}

/// message client channel
#[derive(Debug, Clone)]
pub struct MessageClientChannel<Req, Rep>
where
    Req: Send,
    Rep: Send,
{
    request_channel: crossbeam::Sender<Req>,
    reply_channel: crossbeam::Receiver<Rep>,
}

impl<Req, Rep> MessageClientChannel<Req, Rep>
where
    Req: Send,
    Rep: Send,
{
    /// used by the client to send requests
    pub fn sender(&self) -> &crossbeam::Sender<Req> {
        &self.request_channel
    }

    /// used by the client to receive replies
    pub fn receiver(&self) -> &crossbeam::Receiver<Rep> {
        &self.reply_channel
    }
}

/// Thread config
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ThreadConfig {
    name: String,
    stack_size: Option<usize>,
}

impl ThreadConfig {
    /// constructor
    /// - name is trimmed
    /// - if the name is blank then it will be replaced with a ULID
    ///   - creating a thread with blank name would trigger a panic. In order to avoid to always
    ///     having to handle the error case for this edge case, a random thread name will be used
    pub fn new(name: &str) -> ThreadConfig {
        let name = {
            if name.trim().is_empty() {
                ULID::generate().to_string()
            } else {
                name.to_string()
            }
        };

        ThreadConfig {
            name,
            stack_size: None,
        }
    }

    /// Sets the size of the stack (in bytes) for the new thread.
    /// The actual stack size may be greater than this value if the platform specifies minimal stack size.
    pub fn set_stack_size(self, stack_size: NonZeroUsize) -> ThreadConfig {
        let mut config = self;
        config.stack_size = Some(stack_size.get());
        config
    }

    /// thread builder constructor
    pub fn builder(&self) -> thread::Builder {
        match self.stack_size {
            None => thread::Builder::new().name(self.name.clone()),
            Some(stack_size) => thread::Builder::new()
                .name(self.name.clone())
                .stack_size(stack_size),
        }
    }
}

#[allow(warnings)]
#[cfg(test)]
mod test {
    use super::*;
    use std::{num::NonZeroUsize, time::Duration};

    fn log_config() -> oysterpack_log::LogConfig {
        oysterpack_log::config::LogConfigBuilder::new(oysterpack_log::Level::Info).build()
    }

    #[derive(Clone)]
    struct Echo {
        counter: usize,
    }

    impl Default for Echo {
        fn default() -> Echo {
            Echo { counter: 0 }
        }
    }

    impl MessageProcessor<nng::Message, nng::Message> for Echo {
        fn process(&mut self, req: nng::Message) -> nng::Message {
            self.counter = self.counter + 1;
            info!(
                "received msg #{} on {:?}",
                self.counter,
                thread::current().id()
            );
            thread::sleep_ms(1);
            req
        }
    }

    #[test]
    fn client_service_messaging_with_multi_clients() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);

        let (client, service) = channels::<nng::Message, nng::Message>(10, 10);
        for _ in 0..num_cpus::get() {
            Echo::default().bind(service.clone(), None);
        }
        let msg = b"some data";
        let mut nng_msg = nng::Message::with_capacity(msg.len()).unwrap();
        nng_msg.push_back(&msg[..]);
        client.sender().send(nng_msg);
        let reply = client.receiver().recv().unwrap();
        let msg2 = &**reply;
        info!("{}", std::str::from_utf8(msg2).unwrap());
        assert_eq!(
            std::str::from_utf8(&msg[..]).unwrap(),
            std::str::from_utf8(msg2).unwrap()
        );

        let mut join_handles = vec![];
        for i in 1..=100 {
            let client_2 = client.clone();
            let join_handle = std::thread::spawn(move || {
                let msg = format!("message #{}", i);
                let mut nng_msg = nng::Message::with_capacity(msg.len()).unwrap();
                nng_msg.push_back(msg.as_bytes());
                client_2.sender().send(nng_msg);
                let reply = client_2.receiver().recv().unwrap();
                let msg2 = &**reply;
                println!(
                    "REQ: {} | REPLY: {}",
                    msg,
                    std::str::from_utf8(msg2).unwrap()
                );
            });
            join_handles.push(join_handle);
        }
        join_handles.into_iter().for_each(|handle| {
            handle.join().unwrap();
        });
    }

    #[test]
    fn client_service_messaging_with_thread_config() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);

        let (client, service) = channels::<nng::Message, nng::Message>(10, 10);
        for _ in 0..num_cpus::get() {
            Echo::default().bind(
                service.clone(),
                Some(ThreadConfig::new("Echo").set_stack_size(NonZeroUsize::new(1024).unwrap())),
            );
        }

        let msg = b"some data";
        let mut nng_msg = nng::Message::with_capacity(msg.len()).unwrap();
        nng_msg.push_back(&msg[..]);
        for _ in 0..100 {
            client.sender().send(nng_msg.clone());
            let _ = client.receiver().recv().unwrap();
        }
    }

    #[test]
    fn client_service_messaging_with_thread_config_with_blank_name() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);

        let (client, service) = channels::<nng::Message, nng::Message>(10, 10);
        for _ in 0..num_cpus::get() {
            Echo::default().bind(
                service.clone(),
                Some(ThreadConfig::new("   ").set_stack_size(NonZeroUsize::new(1024).unwrap())),
            );
        }

        let msg = b"some data";
        let mut nng_msg = nng::Message::with_capacity(msg.len()).unwrap();
        nng_msg.push_back(&msg[..]);
        for _ in 0..100 {
            client.sender().send(nng_msg.clone());
            let _ = client.receiver().recv().unwrap();
        }
    }

    #[test]
    fn client_service_messaging_with_thread_config_with_empty_name() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);

        let (client, service) = channels::<nng::Message, nng::Message>(10, 10);
        for _ in 0..num_cpus::get() {
            Echo::default().bind(
                service.clone(),
                Some(ThreadConfig::new("").set_stack_size(NonZeroUsize::new(1024).unwrap())),
            );
        }

        let msg = b"some data";
        let mut nng_msg = nng::Message::with_capacity(msg.len()).unwrap();
        nng_msg.push_back(&msg[..]);
        for _ in 0..100 {
            client.sender().send(nng_msg.clone());
            let _ = client.receiver().recv().unwrap();
        }
    }

    #[test]
    fn message_handler_disconnected() {
        oysterpack_log::init(log_config(), oysterpack_log::StderrLogger);

        let (client, service) = channels::<nng::Message, nng::Message>(10, 10);
        for _ in 0..num_cpus::get() {
            Echo::default().bind(
                service.clone(),
                Some(ThreadConfig::new("Echo").set_stack_size(NonZeroUsize::new(1024).unwrap())),
            );
        }

        fn send_requests(
            client: MessageClientChannel<nng::Message, nng::Message>,
            count: usize,
        ) -> crossbeam::Receiver<nng::Message> {
            let msg = b"some data";
            let mut nng_msg = nng::Message::with_capacity(msg.len()).unwrap();
            nng_msg.push_back(&msg[..]);
            for _ in 0..count {
                client.sender().send(nng_msg.clone()).unwrap();
            }
            info!("all messages have been sent");
            client.receiver().clone()
        }

        let count = 10;
        let reply_channel = send_requests(client, count);
        let _ = reply_channel.recv().unwrap();
        info!("received message");

        // after dropping the request channel, all replies should still continue to flow
        let mut reply_count = 1;
        loop {
            match reply_channel.recv_timeout(Duration::from_millis(100)) {
                Ok(_) => {
                    reply_count = reply_count + 1;
                    info!("received reply #{}", reply_count);
                }
                Err(err) => {
                    error!("Failed to receive message: {}", err);
                    break;
                }
            }
        }
        info!("reply_count = {}", reply_count);
        assert_eq!(reply_count, count);
    }
}