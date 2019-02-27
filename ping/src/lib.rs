mod protocol_builder;
#[rustfmt::skip]
#[allow(clippy::all)]
mod protocol_generated;

use crate::protocol_generated::p2p::ping::*;
use flatbuffers::{get_root, FlatBufferBuilder};
use fnv::FnvHashMap;
use generic_channel::Sender;
use log::{debug, error};
use p2p::{
    context::{ServiceContext, SessionContext},
    traits::{ProtocolMeta, ServiceProtocol},
    PeerId, ProtocolId, SessionId,
};
use std::{
    str,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::codec::length_delimited::LengthDelimitedCodec;

const SEND_PING_TOKEN: u64 = 0;
const CHECK_TIMEOUT_TOKEN: u64 = 1;

/// Ping protocol events
#[derive(Debug)]
pub enum Event {
    /// Peer send ping to us.
    Ping(PeerId),
    /// Peer send pong to us.
    Pong(PeerId, Duration),
    /// Peer is timeout.
    Timeout(PeerId),
    /// Peer cause a unexpected error.
    UnexpectedError(PeerId),
}

pub struct PingProtocol<S: Sender<Event> + Send + Clone> {
    id: ProtocolId,
    /// the interval that we send ping to peers.
    interval: Duration,
    /// consider peer is timeout if during a timeout we still have not received pong from a peer
    timeout: Duration,
    event_sender: S,
}

impl<S> PingProtocol<S>
where
    S: Sender<Event> + Send + Clone,
{
    pub fn new(id: ProtocolId, interval: Duration, timeout: Duration, event_sender: S) -> Self {
        PingProtocol {
            id,
            interval,
            timeout,
            event_sender,
        }
    }
}

impl<S> ProtocolMeta<LengthDelimitedCodec> for PingProtocol<S>
where
    S: Sender<Event> + Send + Clone + 'static,
{
    fn id(&self) -> ProtocolId {
        self.id
    }
    fn codec(&self) -> LengthDelimitedCodec {
        LengthDelimitedCodec::new()
    }

    fn service_handle(&self) -> Option<Box<dyn ServiceProtocol + Send + 'static>> {
        let handle = Box::new(PingHandler {
            proto_id: self.id,
            interval: self.interval,
            timeout: self.timeout,
            connected_session_ids: Default::default(),
            event_sender: self.event_sender.clone(),
        });
        Some(handle)
    }
}

struct PingHandler<S: Sender<Event>> {
    proto_id: ProtocolId,
    interval: Duration,
    timeout: Duration,
    connected_session_ids: FnvHashMap<SessionId, PingStatus>,
    event_sender: S,
}

impl<S: Sender<Event>> PingHandler<S> {
    pub fn send_event(&mut self, event: Event) {
        if let Err(err) = self.event_sender.try_send(event) {
            error!("send ping event error: {}", err);
        }
    }
}

/// PingStatus of a peer
#[derive(Clone, Debug)]
struct PingStatus {
    /// Are we currently pinging this peer?
    processing: bool,
    /// The time we last send ping to this peer.
    last_ping: SystemTime,
    peer_id: PeerId,
}

impl PingStatus {
    /// A meaningless value, peer must send a pong has same nonce to respond a ping.
    fn nonce(&self) -> u32 {
        self.last_ping
            .duration_since(UNIX_EPOCH)
            .map(|dur| dur.as_secs())
            .unwrap_or(0) as u32
    }

    /// Time duration since we last send ping.
    fn elapsed(&self) -> Duration {
        self.last_ping.elapsed().unwrap_or(Duration::from_secs(0))
    }
}

impl<S> ServiceProtocol for PingHandler<S>
where
    S: Sender<Event>,
{
    fn init(&mut self, control: &mut ServiceContext) {
        // periodicly send ping to peers
        control.set_service_notify(self.proto_id, self.interval, SEND_PING_TOKEN);
        control.set_service_notify(self.proto_id, self.timeout, CHECK_TIMEOUT_TOKEN);
    }

    fn connected(&mut self, control: &mut ServiceContext, session: &SessionContext, version: &str) {
        match session.remote_pubkey {
            Some(ref pubkey) => {
                let peer_id = pubkey.peer_id();
                self.connected_session_ids
                    .entry(session.id)
                    .or_insert_with(|| PingStatus {
                        last_ping: SystemTime::now(),
                        processing: false,
                        peer_id,
                    });
                debug!(
                    "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
                    self.proto_id, session.id, session.address, session.ty, version
                );
                debug!("connected sessions are: {:?}", self.connected_session_ids);
            }
            None => {
                control.disconnect(session.id);
            }
        }
    }

    fn disconnected(&mut self, _control: &mut ServiceContext, session: &SessionContext) {
        self.connected_session_ids.remove(&session.id);
        debug!(
            "proto id [{}] close on session [{}]",
            self.proto_id, session.id
        );
    }

    fn received(&mut self, control: &mut ServiceContext, session: &SessionContext, data: Vec<u8>) {
        if let Some(peer_id) = self
            .connected_session_ids
            .get(&session.id)
            .map(|ps| ps.peer_id.clone())
        {
            let msg = get_root::<PingMessage>(&data);
            match msg.payload_type() {
                PingPayload::Ping => {
                    let ping_msg = msg.payload_as_ping().unwrap();
                    let mut fbb = FlatBufferBuilder::new();
                    let msg = PingMessage::build_pong(&mut fbb, ping_msg.nonce());
                    fbb.finish(msg, None);
                    control.send_message(session.id, self.proto_id, fbb.finished_data().to_vec());
                    self.send_event(Event::Ping(peer_id));
                }
                PingPayload::Pong => {
                    let pong_msg = msg.payload_as_pong().unwrap();
                    // check pong
                    if self
                        .connected_session_ids
                        .get(&session.id)
                        .map(|ps| (ps.processing, ps.nonce()))
                        == Some((true, pong_msg.nonce()))
                    {
                        let ping_time = match self.connected_session_ids.get_mut(&session.id) {
                            Some(ps) => {
                                ps.processing = false;
                                ps.elapsed()
                            }
                            None => return,
                        };
                        self.send_event(Event::Pong(peer_id, ping_time));
                    } else {
                        // ignore if nonce is incorrect
                        self.send_event(Event::UnexpectedError(peer_id));
                    }
                }
                PingPayload::NONE => {
                    // can't decode msg
                    self.send_event(Event::UnexpectedError(peer_id));
                }
            }
        }
    }

    fn notify(&mut self, control: &mut ServiceContext, token: u64) {
        match token {
            SEND_PING_TOKEN => {
                debug!("proto [{}] start ping peers", self.proto_id);
                let now = SystemTime::now();
                let peers: Vec<(SessionId, u32)> = self
                    .connected_session_ids
                    .iter_mut()
                    .filter_map(|(session_id, ps)| {
                        if ps.processing {
                            None
                        } else {
                            ps.processing = true;
                            ps.last_ping = now;
                            Some((*session_id, ps.nonce()))
                        }
                    })
                    .collect();
                if !peers.is_empty() {
                    let mut fbb = FlatBufferBuilder::new();
                    let msg = PingMessage::build_ping(&mut fbb, peers[0].1);
                    fbb.finish(msg, None);
                    let peer_ids: Vec<SessionId> = peers
                        .into_iter()
                        .map(|(session_id, _)| session_id)
                        .collect();
                    control.filter_broadcast(
                        Some(peer_ids),
                        self.proto_id,
                        fbb.finished_data().to_vec(),
                    );
                }
            }
            CHECK_TIMEOUT_TOKEN => {
                debug!("proto [{}] check ping timeout", self.proto_id);
                let timeout = self.timeout;
                for peer_id in self
                    .connected_session_ids
                    .values()
                    .filter(|ps| ps.processing && ps.elapsed() >= timeout)
                    .map(|ps| ps.peer_id.clone())
                    .collect::<Vec<PeerId>>()
                {
                    self.send_event(Event::Timeout(peer_id));
                }
            }
            _ => panic!("unknown token {}", token),
        }
    }
}
