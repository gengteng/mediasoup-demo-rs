#![allow(dead_code)]

use crate::room::Room;
use axum::extract::ws::{Message, WebSocket};
use event_listener_primitives::HandlerId;
use mediasoup::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

pub mod messages {
    use crate::participant::ParticipantId;
    use crate::room::RoomId;
    use mediasoup::prelude::*;
    use serde::{Deserialize, Serialize};

    /// Data structure containing all the necessary information about transport options required
    /// from the server to establish transport connection on the client
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TransportOptions {
        pub id: TransportId,
        pub dtls_parameters: DtlsParameters,
        pub ice_candidates: Vec<IceCandidate>,
        pub ice_parameters: IceParameters,
    }

    /// Server messages sent to the client
    #[derive(Serialize)]
    #[serde(tag = "action")]
    pub enum ServerMessage {
        /// Initialization message with consumer/producer transport options and Router's RTP
        /// capabilities necessary to establish WebRTC transport connection client-side
        #[serde(rename_all = "camelCase")]
        Init {
            room_id: RoomId,
            consumer_transport_options: TransportOptions,
            producer_transport_options: TransportOptions,
            router_rtp_capabilities: RtpCapabilitiesFinalized,
        },
        /// Notification that new producer was added to the room
        #[serde(rename_all = "camelCase")]
        ProducerAdded {
            participant_id: ParticipantId,
            producer_id: ProducerId,
        },
        /// Notification that producer was removed from the room
        #[serde(rename_all = "camelCase")]
        ProducerRemoved {
            participant_id: ParticipantId,
            producer_id: ProducerId,
        },
        /// Notification that producer transport was connected successfully (in case of error
        /// connection is just dropped, in real-world application you probably want to handle it
        /// better)
        ConnectedProducerTransport,
        /// Notification that producer was created on the server
        #[serde(rename_all = "camelCase")]
        Produced { id: ProducerId },
        /// Notification that consumer transport was connected successfully (in case of error
        /// connection is just dropped, in real-world application you probably want to handle it
        /// better)
        ConnectedConsumerTransport,
        /// Notification that consumer was successfully created server-side, client can resume
        /// the consumer after this
        #[serde(rename_all = "camelCase")]
        Consumed {
            id: ConsumerId,
            producer_id: ProducerId,
            kind: MediaKind,
            rtp_parameters: RtpParameters,
        },
    }

    /// Client messages sent to the server
    #[derive(Deserialize)]
    #[serde(tag = "action")]
    pub enum ClientMessage {
        /// Client-side initialization with its RTP capabilities, in this simple case we expect
        /// those to match server Router's RTP capabilities
        #[serde(rename_all = "camelCase")]
        Init { rtp_capabilities: RtpCapabilities },
        /// Request to connect producer transport with client-side DTLS parameters
        #[serde(rename_all = "camelCase")]
        ConnectProducerTransport { dtls_parameters: DtlsParameters },
        /// Request to produce a new audio or video track with specified RTP parameters
        #[serde(rename_all = "camelCase")]
        Produce {
            kind: MediaKind,
            rtp_parameters: RtpParameters,
        },
        /// Request to connect consumer transport with client-side DTLS parameters
        #[serde(rename_all = "camelCase")]
        ConnectConsumerTransport { dtls_parameters: DtlsParameters },
        /// Request to consume specified producer
        #[serde(rename_all = "camelCase")]
        Consume { producer_id: ProducerId },
        /// Request to resume consumer that was previously created
        #[serde(rename_all = "camelCase")]
        ConsumerResume { id: ConsumerId },
    }

    /// Internal actor messages for convenience
    #[derive(Debug)]
    pub enum InternalMessage {
        /// Save producer in connection-specific hashmap to prevent it from being destroyed
        SaveProducer(Producer),
        /// Save consumer in connection-specific hashmap to prevent it from being destroyed
        SaveConsumer(Consumer),
        /// Stop/close the WebSocket connection
        Stop,
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Deserialize, Serialize)]
pub struct ParticipantId(Uuid);

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl ParticipantId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Consumer/producer transports pair for the client
struct Transports {
    consumer: WebRtcTransport,
    producer: WebRtcTransport,
}

/// Actor that will represent WebSocket connection from the client, it will handle inbound and
/// outbound WebSocket messages in JSON.
///
/// See https://actix.rs/docs/websockets/ for official `actix-web` documentation.
pub struct ParticipantConnection {
    id: ParticipantId,
    /// RTP capabilities received from the client
    client_rtp_capabilities: Option<RtpCapabilities>,
    /// Consumers associated with this client, preventing them from being destroyed
    consumers: HashMap<ConsumerId, Consumer>,
    /// Producers associated with this client, preventing them from being destroyed
    producers: Vec<Producer>,
    /// Consumer and producer transports associated with this client
    transports: Transports,
    /// Room to which the client belongs
    room: Room,
    /// Event handlers that were attached and need to be removed when participant connection is
    /// destroyed
    attached_handlers: Vec<HandlerId>,
}

impl Drop for ParticipantConnection {
    fn drop(&mut self) {
        self.room.remove_participant(&self.id);
    }
}

impl ParticipantConnection {
    /// Create a new instance representing WebSocket connection
    pub async fn new(room: Room) -> anyhow::Result<Self> {
        // We know that for videoroom example we'll need 2 transports, so we can create both
        // right away. This may not be the case for real-world applications or you may create
        // this at a different time and/or in different order.
        let transport_options =
            WebRtcTransportOptions::new(TransportListenIps::new(TransportListenIp {
                ip: "127.0.0.1".parse().unwrap(),
                announced_ip: None,
            }));
        let producer_transport = room
            .router()
            .create_webrtc_transport(transport_options.clone())
            .await
            .map_err(|error| anyhow::anyhow!("Failed to create producer transport: {}", error))?;

        let consumer_transport = room
            .router()
            .create_webrtc_transport(transport_options)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to create consumer transport: {}", error))?;

        Ok(Self {
            id: ParticipantId::new(),
            client_rtp_capabilities: None,
            consumers: HashMap::new(),
            producers: vec![],
            transports: Transports {
                consumer: consumer_transport,
                producer: producer_transport,
            },
            room,
            attached_handlers: Vec::new(),
        })
    }

    pub async fn run(mut self, mut socket: WebSocket) -> anyhow::Result<()> {
        use messages::*;

        let server_init_message = ServerMessage::Init {
            room_id: self.room.id(),
            consumer_transport_options: TransportOptions {
                id: self.transports.consumer.id(),
                dtls_parameters: self.transports.consumer.dtls_parameters(),
                ice_candidates: self.transports.consumer.ice_candidates().clone(),
                ice_parameters: self.transports.consumer.ice_parameters().clone(),
            },
            producer_transport_options: TransportOptions {
                id: self.transports.producer.id(),
                dtls_parameters: self.transports.producer.dtls_parameters(),
                ice_candidates: self.transports.producer.ice_candidates().clone(),
                ice_parameters: self.transports.producer.ice_parameters().clone(),
            },
            router_rtp_capabilities: self.room.router().rtp_capabilities().clone(),
        };

        socket
            .send(Message::Text(serde_json::to_string(&server_init_message)?))
            .await?;

        let (server_message_sender, mut server_message_receiver) =
            unbounded_channel::<ServerMessage>();

        // Listen for new producers added to the room
        self.attached_handlers.push(self.room.on_producer_add({
            let own_participant_id = self.id;
            let tx = server_message_sender.clone();
            move |participant_id, producer| {
                if &own_participant_id == participant_id {
                    return;
                }
                if let Err(e) = tx.send(ServerMessage::ProducerAdded {
                    participant_id: *participant_id,
                    producer_id: producer.id(),
                }) {
                    log::error!("Failed to send server message (new producer): {}", e);
                }
            }
        }));
        // Listen for producers removed from the the room
        self.attached_handlers.push(self.room.on_producer_remove({
            let own_participant_id = self.id;
            let tx = server_message_sender.clone();
            move |participant_id, producer_id| {
                if &own_participant_id == participant_id {
                    return;
                }
                if let Err(e) = tx.send(ServerMessage::ProducerRemoved {
                    participant_id: *participant_id,
                    producer_id: *producer_id,
                }) {
                    log::error!("Failed to send server message (producer removed): {}", e);
                }
            }
        }));
        // Notify client about any producers that already exist in the room
        for (participant_id, producer_id) in self.room.get_all_producers() {
            if let Err(e) = server_message_sender.send(ServerMessage::ProducerAdded {
                participant_id,
                producer_id,
            }) {
                log::warn!(
                    "Failed to send server message (to notify client producers): {}",
                    e
                );
            }
        }

        let (internal_message_sender, mut internal_message_receiver) =
            unbounded_channel::<InternalMessage>();

        loop {
            tokio::select! {
                internal_message_recv = internal_message_receiver.recv() => {
                    use messages::InternalMessage;
                    if let Some(message) = internal_message_recv {
                        match message {
                            InternalMessage::Stop => {
                                break;
                            }
                            InternalMessage::SaveProducer(producer) => {
                                // Retain producer to prevent it from being destroyed
                                self.producers.push(producer);
                            }
                            InternalMessage::SaveConsumer(consumer) => {
                                self.consumers.insert(consumer.id(), consumer);
                            }
                        }
                    }
                }
                server_message_recv = server_message_receiver.recv() => {
                    if let Some(message) = server_message_recv {
                        if let Err(e) = socket.send(Message::Text(serde_json::to_string(&message)?)).await {
                            log::error!("send server message error: {}", e);
                            internal_message_sender.send(InternalMessage::Stop).unwrap_or_default();
                        }
                    }
                }
                websocket_recv = socket.recv() => {
                    if let Some(result) = websocket_recv {
                        let message = result?;
                        use messages::ClientMessage;
                        match message {
                            Message::Text(text) => {
                                let client_message: ClientMessage =
                                    serde_json::from_str(&text)?;

                                match client_message {
                                    ClientMessage::Init { rtp_capabilities } => {
                                        self.client_rtp_capabilities.replace(rtp_capabilities);
                                    }
                                    ClientMessage::ConnectProducerTransport { dtls_parameters } => {
                                        let participant_id = self.id;
                                        let transport = self.transports.producer.clone();
                                        // Establish connection for producer transport using DTLS parameters received
                                        // from the client, but doing so in a background task since this handler is
                                        // synchronous
                                        let internal_sender = internal_message_sender.clone();
                                        let server_sender = server_message_sender.clone();
                                        tokio::spawn(async move {
                                            match transport
                                                .connect(WebRtcTransportRemoteParameters { dtls_parameters })
                                                .await
                                            {
                                                Ok(_) => {
                                                    if let Err(e) = server_sender.send(ServerMessage::ConnectedProducerTransport) {
                                                        log::error!("send message error: {}", e);
                                                        internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                    }
                                                    log::info!(
                                                        "[participant_id {}] Producer transport connected",
                                                        participant_id,
                                                    );
                                                }
                                                Err(error) => {
                                                    log::error!("Failed to connect producer transport: {}", error);
                                                    internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                }
                                            }
                                        });
                                    }
                                    ClientMessage::Produce { kind, rtp_parameters, } => {
                                        log::debug!("Received client message 'Produce'.");
                                        let participant_id = self.id;

                                        let transport = self.transports.producer.clone();
                                        let room = self.room.clone();
                                        // Use producer transport to create a new producer on the server with given RTP
                                        // parameters
                                        let server_sender = server_message_sender.clone();
                                        let internal_sender = internal_message_sender.clone();

                                        std::thread::spawn(move || {
                                            futures::executor::block_on(async move {
                                                log::debug!("Trying to produce");
                                                match transport
                                                    .produce(ProducerOptions::new(kind, rtp_parameters))
                                                    .await
                                                {
                                                    Ok(producer) => {
                                                        let id = producer.id();
                                                        if let Err(e) = server_sender.send(ServerMessage::Produced { id }) {
                                                            log::error!("send message error: {}", e);
                                                            internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                            return;
                                                        }
                                                        // Add producer to the room so that others can consume it
                                                        room.add_producer(participant_id, producer.clone());
                                                        // Producer is stored in a hashmap since if we don't do it, it will
                                                        // get destroyed as soon as its instance goes out out scope
                                                        internal_sender.send(InternalMessage::SaveProducer(producer)).unwrap_or_default();
                                                        log::info!(
                                                            "[participant_id {}] {:?} producer created: {}",
                                                            participant_id, kind, id,
                                                        );
                                                    }
                                                    Err(error) => {
                                                        log::error!(
                                                            "[participant_id {}] Failed to create {:?} producer: {}",
                                                            participant_id, kind, error
                                                        );
                                                        internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                    }
                                                }
                                            });
                                        });
                                    }
                                    ClientMessage::ConnectConsumerTransport { dtls_parameters} => {
                                        let participant_id = self.id;
                                        let transport = self.transports.consumer.clone();
                                        // The same as producer transport, but for consumer transport

                                        let server_sender = server_message_sender.clone();
                                        let internal_sender = internal_message_sender.clone();
                                        tokio::spawn(async move {
                                            match transport
                                                .connect(WebRtcTransportRemoteParameters { dtls_parameters })
                                                .await
                                            {
                                                Ok(_) => {
                                                    if let Err(e) = server_sender.send(ServerMessage::ConnectedConsumerTransport) {
                                                        log::error!("send message error: {}", e);
                                                        internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                        return;
                                                    }
                                                    log::info!(
                                                        "[participant_id {}] Consumer transport connected",
                                                        participant_id,
                                                    );
                                                }
                                                Err(error) => {
                                                    log::error!(
                                                        "[participant_id {}] Failed to connect consumer transport: {}",
                                                        participant_id, error,
                                                    );
                                                    internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                }
                                            }
                                        });
                                    }
                                    ClientMessage::Consume { producer_id } => {
                                        let participant_id = self.id;
                                        let transport = self.transports.consumer.clone();
                                        let rtp_capabilities = match self.client_rtp_capabilities.clone() {
                                            Some(rtp_capabilities) => rtp_capabilities,
                                            None => {
                                                log::error!(
                                                    "[participant_id {}] Client should send RTP capabilities before \
                                                    consuming",
                                                    participant_id,
                                                );
                                                continue;
                                            }
                                        };
                                        // Create consumer for given producer ID, while first making sure that RTP
                                        // capabilities were sent by the client prior to that
                                        let server_sender = server_message_sender.clone();
                                        let internal_sender = internal_message_sender.clone();
                                        std::thread::spawn(move || {
                                            futures::executor::block_on(async move {
                                                let mut options = ConsumerOptions::new(producer_id, rtp_capabilities);
                                                options.paused = true;

                                                match transport.consume(options).await {
                                                    Ok(consumer) => {
                                                        let id = consumer.id();
                                                        let kind = consumer.kind();
                                                        let rtp_parameters = consumer.rtp_parameters().clone();
                                                        if let Err(e) = server_sender.send(ServerMessage::Consumed {
                                                            id,
                                                            producer_id,
                                                            kind,
                                                            rtp_parameters,
                                                        }) {
                                                            log::error!("send message error: {}", e);
                                                            internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                            return;
                                                        }
                                                        // Consumer is stored in a hashmap since if we don't do it, it will
                                                        // get destroyed as soon as its instance goes out out scope
                                                        internal_sender.send(InternalMessage::SaveConsumer(consumer)).unwrap_or_default();
                                                        log::info!(
                                                            "[participant_id {}] {:?} consumer created: {}",
                                                            participant_id, kind, id,
                                                        );
                                                    }
                                                    Err(error) => {
                                                        log::error!(
                                                            "[participant_id {}] Failed to create consumer: {}",
                                                            participant_id, error,
                                                        );
                                                        internal_sender.send(InternalMessage::Stop).unwrap_or_default();
                                                    }
                                                }
                                            })
                                        });
                                    }
                                    ClientMessage::ConsumerResume { id } => {
                                        if let Some(consumer) = self.consumers.get(&id).cloned() {
                                            let participant_id = self.id;
                                            tokio::spawn(async move {
                                                match consumer.resume().await {
                                                    Ok(_) => {
                                                        log::info!(
                                                            "[participant_id {}] Successfully resumed {:?} consumer {}",
                                                            participant_id,
                                                            consumer.kind(),
                                                            consumer.id(),
                                                        );
                                                    }
                                                    Err(error) => {
                                                        log::error!(
                                                            "[participant_id {}] Failed to resume {:?} consumer {}: {}",
                                                            participant_id,
                                                            consumer.kind(),
                                                            consumer.id(),
                                                            error,
                                                        );
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                            Message::Binary(bytes) => {
                                log::warn!("Unexpected binary message ({} bytes): {:?}", bytes.len(), bytes);
                            }
                            Message::Ping(bytes) => {
                                socket.send(Message::Pong(bytes)).await?;
                            }
                            Message::Pong(_) => {}
                            Message::Close(close) => {
                                if let Some(close_frame) = close {
                                    log::debug!(
                                        "Received close frame: ({}, {})",
                                        close_frame.code,
                                        close_frame.reason
                                    );
                                }
                                socket.close().await?;
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
//
// impl Actor for ParticipantConnection {
//     type Context = ws::WebsocketContext<Self>;
//
//     fn started(&mut self, ctx: &mut Self::Context) {
//         println!("[participant_id {}] WebSocket connection created", self.id);
//
//         // We know that both consumer and producer transports will be used, so we sent server
//         // information about both in an initialization message alongside with router
//         // capabilities to the client right after WebSocket connection is established
//         let server_init_message = ServerMessage::Init {
//             room_id: self.room.id(),
//             consumer_transport_options: TransportOptions {
//                 id: self.transports.consumer.id(),
//                 dtls_parameters: self.transports.consumer.dtls_parameters(),
//                 ice_candidates: self.transports.consumer.ice_candidates().clone(),
//                 ice_parameters: self.transports.consumer.ice_parameters().clone(),
//             },
//             producer_transport_options: TransportOptions {
//                 id: self.transports.producer.id(),
//                 dtls_parameters: self.transports.producer.dtls_parameters(),
//                 ice_candidates: self.transports.producer.ice_candidates().clone(),
//                 ice_parameters: self.transports.producer.ice_parameters().clone(),
//             },
//             router_rtp_capabilities: self.room.router().rtp_capabilities().clone(),
//         };
//
//         let address = ctx.address();
//         address.do_send(server_init_message);
//
//         // Listen for new producers added to the room
//         self.attached_handlers.push(self.room.on_producer_add({
//             let own_participant_id = self.id;
//             let address = address.clone();
//
//             move |participant_id, producer| {
//                 if &own_participant_id == participant_id {
//                     return;
//                 }
//                 address.do_send(ServerMessage::ProducerAdded {
//                     participant_id: *participant_id,
//                     producer_id: producer.id(),
//                 });
//             }
//         }));
//
//         // Listen for producers removed from the the room
//         self.attached_handlers.push(self.room.on_producer_remove({
//             let own_participant_id = self.id;
//             let address = address.clone();
//
//             move |participant_id, producer_id| {
//                 if &own_participant_id == participant_id {
//                     return;
//                 }
//                 address.do_send(ServerMessage::ProducerRemoved {
//                     participant_id: *participant_id,
//                     producer_id: *producer_id,
//                 });
//             }
//         }));
//
//         // Notify client about any producers that already exist in the room
//         for (participant_id, producer_id) in self.room.get_all_producers() {
//             address.do_send(ServerMessage::ProducerAdded {
//                 participant_id,
//                 producer_id,
//             });
//         }
//     }
//
//     fn stopped(&mut self, _ctx: &mut Self::Context) {
//         println!("[participant_id {}] WebSocket connection closed", self.id);
//     }
// }
//
// impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for ParticipantConnection {
//     fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
//         // Here we handle incoming WebSocket messages, intentionally not handling continuation
//         // messages since we know all messages will fit into a single frame, but in real-world
//         // apps you need to handle continuation frames too (`ws::Message::Continuation`)
//         match msg {
//             Ok(ws::Message::Ping(msg)) => {
//                 ctx.pong(&msg);
//             }
//             Ok(ws::Message::Pong(_)) => {}
//             Ok(ws::Message::Text(text)) => match serde_json::from_str::<ClientMessage>(&text) {
//                 Ok(message) => {
//                     // Parse JSON into an enum and just send it back to the actor to be
//                     // processed by another handler below, it is much more convenient to just
//                     // parse it in one place and have typed data structure everywhere else
//                     ctx.address().do_send(message);
//                 }
//                 Err(error) => {
//                     eprintln!("Failed to parse client message: {}\n{}", error, text);
//                 }
//             },
//             Ok(ws::Message::Binary(bin)) => {
//                 eprintln!("Unexpected binary message: {:?}", bin);
//             }
//             Ok(ws::Message::Close(reason)) => {
//                 ctx.close(reason);
//                 ctx.stop();
//             }
//             _ => ctx.stop(),
//         }
//     }
// }
//
// impl Handler<ClientMessage> for ParticipantConnection {
//     type Result = ();
//
//     fn handle(&mut self, message: ClientMessage, ctx: &mut Self::Context) {
//         match message {
//             ClientMessage::Init { rtp_capabilities } => {
//                 // We need to know client's RTP capabilities, those are sent using
//                 // initialization message and are stored in connection struct for future use
//                 self.client_rtp_capabilities.replace(rtp_capabilities);
//             }
//             ClientMessage::ConnectProducerTransport { dtls_parameters } => {
//                 let participant_id = self.id;
//                 let address = ctx.address();
//                 let transport = self.transports.producer.clone();
//                 // Establish connection for producer transport using DTLS parameters received
//                 // from the client, but doing so in a background task since this handler is
//                 // synchronous
//                 actix::spawn(async move {
//                     match transport
//                         .connect(WebRtcTransportRemoteParameters { dtls_parameters })
//                         .await
//                     {
//                         Ok(_) => {
//                             address.do_send(ServerMessage::ConnectedProducerTransport);
//                             println!(
//                                 "[participant_id {}] Producer transport connected",
//                                 participant_id,
//                             );
//                         }
//                         Err(error) => {
//                             eprintln!("Failed to connect producer transport: {}", error);
//                             address.do_send(InternalMessage::Stop);
//                         }
//                     }
//                 });
//             }
//             ClientMessage::Produce {
//                 kind,
//                 rtp_parameters,
//             } => {
//                 let participant_id = self.id;
//                 let address = ctx.address();
//                 let transport = self.transports.producer.clone();
//                 let room = self.room.clone();
//                 // Use producer transport to create a new producer on the server with given RTP
//                 // parameters
//                 actix::spawn(async move {
//                     match transport
//                         .produce(ProducerOptions::new(kind, rtp_parameters))
//                         .await
//                     {
//                         Ok(producer) => {
//                             let id = producer.id();
//                             address.do_send(ServerMessage::Produced { id });
//                             // Add producer to the room so that others can consume it
//                             room.add_producer(participant_id, producer.clone());
//                             // Producer is stored in a hashmap since if we don't do it, it will
//                             // get destroyed as soon as its instance goes out out scope
//                             address.do_send(InternalMessage::SaveProducer(producer));
//                             println!(
//                                 "[participant_id {}] {:?} producer created: {}",
//                                 participant_id, kind, id,
//                             );
//                         }
//                         Err(error) => {
//                             eprintln!(
//                                 "[participant_id {}] Failed to create {:?} producer: {}",
//                                 participant_id, kind, error
//                             );
//                             address.do_send(InternalMessage::Stop);
//                         }
//                     }
//                 });
//             }
//             ClientMessage::ConnectConsumerTransport { dtls_parameters } => {
//                 let participant_id = self.id;
//                 let address = ctx.address();
//                 let transport = self.transports.consumer.clone();
//                 // The same as producer transport, but for consumer transport
//                 actix::spawn(async move {
//                     match transport
//                         .connect(WebRtcTransportRemoteParameters { dtls_parameters })
//                         .await
//                     {
//                         Ok(_) => {
//                             address.do_send(ServerMessage::ConnectedConsumerTransport);
//                             println!(
//                                 "[participant_id {}] Consumer transport connected",
//                                 participant_id,
//                             );
//                         }
//                         Err(error) => {
//                             eprintln!(
//                                 "[participant_id {}] Failed to connect consumer transport: {}",
//                                 participant_id, error,
//                             );
//                             address.do_send(InternalMessage::Stop);
//                         }
//                     }
//                 });
//             }
//             ClientMessage::Consume { producer_id } => {
//                 let participant_id = self.id;
//                 let address = ctx.address();
//                 let transport = self.transports.consumer.clone();
//                 let rtp_capabilities = match self.client_rtp_capabilities.clone() {
//                     Some(rtp_capabilities) => rtp_capabilities,
//                     None => {
//                         eprintln!(
//                             "[participant_id {}] Client should send RTP capabilities before \
//                                 consuming",
//                             participant_id,
//                         );
//                         return;
//                     }
//                 };
//                 // Create consumer for given producer ID, while first making sure that RTP
//                 // capabilities were sent by the client prior to that
//                 actix::spawn(async move {
//                     let mut options = ConsumerOptions::new(producer_id, rtp_capabilities);
//                     options.paused = true;
//
//                     match transport.consume(options).await {
//                         Ok(consumer) => {
//                             let id = consumer.id();
//                             let kind = consumer.kind();
//                             let rtp_parameters = consumer.rtp_parameters().clone();
//                             address.do_send(ServerMessage::Consumed {
//                                 id,
//                                 producer_id,
//                                 kind,
//                                 rtp_parameters,
//                             });
//                             // Consumer is stored in a hashmap since if we don't do it, it will
//                             // get destroyed as soon as its instance goes out out scope
//                             address.do_send(InternalMessage::SaveConsumer(consumer));
//                             println!(
//                                 "[participant_id {}] {:?} consumer created: {}",
//                                 participant_id, kind, id,
//                             );
//                         }
//                         Err(error) => {
//                             eprintln!(
//                                 "[participant_id {}] Failed to create consumer: {}",
//                                 participant_id, error,
//                             );
//                             address.do_send(InternalMessage::Stop);
//                         }
//                     }
//                 });
//             }
//             ClientMessage::ConsumerResume { id } => {
//                 if let Some(consumer) = self.consumers.get(&id).cloned() {
//                     let participant_id = self.id;
//                     actix::spawn(async move {
//                         match consumer.resume().await {
//                             Ok(_) => {
//                                 println!(
//                                     "[participant_id {}] Successfully resumed {:?} consumer {}",
//                                     participant_id,
//                                     consumer.kind(),
//                                     consumer.id(),
//                                 );
//                             }
//                             Err(error) => {
//                                 println!(
//                                     "[participant_id {}] Failed to resume {:?} consumer {}: {}",
//                                     participant_id,
//                                     consumer.kind(),
//                                     consumer.id(),
//                                     error,
//                                 );
//                             }
//                         }
//                     });
//                 }
//             }
//         }
//     }
// }
//
// /// Simple handler that will transform typed server messages into JSON and send them over to the
// /// client over WebSocket connection
// impl Handler<ServerMessage> for ParticipantConnection {
//     type Result = ();
//
//     fn handle(&mut self, message: ServerMessage, ctx: &mut Self::Context) {
//         ctx.text(serde_json::to_string(&message).unwrap());
//     }
// }
//
// /// Convenience handler for internal messages, these actions require mutable access to the
// /// connection struct and having such message handler makes it easy to use from background tasks
// /// where otherwise Mutex would have to be used instead
// impl Handler<InternalMessage> for ParticipantConnection {
//     type Result = ();
//
//     fn handle(&mut self, message: InternalMessage, ctx: &mut Self::Context) {
//         match message {
//             InternalMessage::Stop => {
//                 ctx.stop();
//             }
//             InternalMessage::SaveProducer(producer) => {
//                 // Retain producer to prevent it from being destroyed
//                 self.producers.push(producer);
//             }
//             InternalMessage::SaveConsumer(consumer) => {
//                 self.consumers.insert(consumer.id(), consumer);
//             }
//         }
//     }
// }
