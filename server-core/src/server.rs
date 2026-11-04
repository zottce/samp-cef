use crossbeam_channel::{Receiver, Sender};
use messages::{packets, try_into_packet};
use network::{CertStrategy, Event as SocketEvent, PeerId, Socket};
use quick_protobuf::deserialize_from_slice;
use tracing::trace;

use std::collections::HashMap;
use std::convert::TryInto;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// VSL patch (2026-11-04): the network thread must stop before the plugin DLL is unloaded at server
// exit, otherwise the process crashes ("instruction at 0x0") after the gamemode already shut down.
static STOP: AtomicBool = AtomicBool::new(false);
static WORKER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// stop the network thread and wait for it (the socket and its runtime close with it)
pub fn shutdown() {
    STOP.store(true, Ordering::Release);

    let handle = WORKER.lock().ok().and_then(|mut worker| worker.take());

    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

use crate::ServerEvent;
use crate::client::Client;
use crate::client::State;

enum Packet {
    Normal { peer: PeerId, bytes: Vec<u8> },
    Disconnect(PeerId),
}

impl Packet {
    fn new(peer: PeerId, bytes: Vec<u8>) -> Packet {
        Packet::Normal { peer, bytes }
    }

    fn disconnect(peer: PeerId) -> Packet {
        Packet::Disconnect(peer)
    }
}

pub struct Server {
    event_tx: Sender<ServerEvent>,
    event_rx: Receiver<ServerEvent>,
    sender: Sender<Packet>,
    allowed: HashMap<IpAddr, i32>,
    clients: HashMap<PeerId, Client>,
    players: HashMap<i32, PeerId>,
}

impl Server {
    pub fn new(addr: SocketAddr) -> Result<Arc<Mutex<Server>>, String> {
        let mut socket = Socket::new_server(addr, CertStrategy::SelfSigned)
            .map_err(|error| format!("failed to bind CEF server on {addr}: {error}"))?;

        let (sender, receiver) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let server = Server {
            event_tx,
            event_rx,
            sender,
            allowed: HashMap::new(),
            clients: HashMap::new(),
            players: HashMap::new(),
        };

        let server = Arc::new(Mutex::new(server));
        let server_clone = server.clone();

        let handle = std::thread::spawn(move || {
            while !STOP.load(Ordering::Acquire) {
                while let Some(event) = socket.recv() {
                    match event {
                        SocketEvent::Message(peer, bytes) => {
                            if let Ok(proto) = deserialize_from_slice::<packets::Packet>(&bytes) {
                                let mut server = server.lock().unwrap();
                                server.handle_client_packet(peer, proto);
                            }
                        }

                        SocketEvent::Connected(peer, addr) => {
                            let mut server = server.lock().unwrap();
                            server.handle_new_connection(peer, addr);
                        }

                        SocketEvent::Disconnect(peer, _) => {
                            let mut server = server.lock().unwrap();
                            server.handle_timeout(peer);
                        }

                        _ => (),
                    }
                }

                for packet in receiver.try_iter() {
                    match packet {
                        Packet::Normal { peer, bytes } => socket.send_message(peer, bytes),
                        Packet::Disconnect(peer) => {
                            trace!("socket::disconnect {:?}", peer);
                            socket.disconnect(peer);
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(5));
            }
        });

        if let Ok(mut worker) = WORKER.lock() {
            *worker = Some(handle);
        }

        Ok(server_clone)
    }

    fn handle_client_packet(&mut self, peer: PeerId, packet: packets::Packet) {
        use messages::packets::PacketId;

        if !self.clients.contains_key(&peer) {
            return;
        }

        match packet.packet_id {
            PacketId::REQUEST_JOIN => {
                let _ = deserialize_from_slice(&packet.bytes)
                    .map(|packet| self.handle_auth(peer, packet));
            }

            PacketId::EMIT_EVENT => {
                let _ = deserialize_from_slice(&packet.bytes)
                    .map(|packet| self.handle_emit_event(peer, packet));
            }

            PacketId::BROWSER_CREATED => {
                let _ = deserialize_from_slice(&packet.bytes)
                    .map(|packet| self.handle_browser_created(peer, packet));
            }

            _ => (),
        }
    }

    fn handle_auth(&mut self, peer: PeerId, _packet: packets::RequestJoin) {
        let client = self.clients.get_mut(&peer).unwrap();

        let response = packets::JoinResponse {
            success: true,
            current_version: None,
        };

        client.set_state(State::Connected);

        let _ = self
            .event_tx
            .send(ServerEvent::PlayerConnected(client.id()));

        let _ = try_into_packet(response).map(|bytes| {
            let packet = Packet::new(peer, bytes);
            let _ = self.sender.send(packet);
        });
    }

    fn handle_emit_event(&mut self, peer: PeerId, packet: packets::EmitEvent) {
        let client = self.clients.get_mut(&peer).unwrap();
        let player_id = client.id();

        if let Some(args) = &packet.args {
            let event = packet.event_name.to_string();
            let arguments = args.to_string();
            let event = ServerEvent::EmitEvent {
                player_id,
                arguments,
                event,
            };

            let _ = self.event_tx.send(event);
        }
    }

    fn handle_browser_created(&mut self, peer: PeerId, packet: packets::BrowserCreated) {
        let client = self.clients.get_mut(&peer).unwrap();
        let player_id = client.id();

        let event = ServerEvent::BrowserCreated {
            player_id,
            browser_id: packet.browser_id,
            code: packet.status_code,
        };

        let _ = self.event_tx.send(event);
    }

    fn handle_timeout(&mut self, addr: PeerId) {
        trace!("handle_timeout {:?}", addr);
        if let Some(client) = self.clients.remove(&addr) {
            self.players.remove(&client.id());
        }

        trace!("{:#?}", self.allowed);
        trace!("{:#?}", self.clients);
    }

    fn handle_new_connection(&mut self, peer: PeerId, addr: SocketAddr) {
        trace!("handle_new_connection {:?} {:?}", peer, addr);

        if !self.clients.contains_key(&peer) && self.allowed.contains_key(&addr.ip()) {
            let player_id = *self.allowed.get(&addr.ip()).unwrap();

            trace!("handle_new_connection: ok {}", player_id);

            if self.peer_by_id(player_id).is_none() {
                trace!("handle_new_connection: ok no peer with this id");

                let client = Client::new(player_id, peer, addr);

                self.clients.insert(peer, client);
                self.players.insert(player_id, peer);

                let request = packets::OpenConnection {};

                let _ = try_into_packet(request).map(|bytes| {
                    let packet = Packet::new(peer, bytes);
                    let _ = self.sender.send(packet);
                });

                return;
            }
        }

        let packet = Packet::disconnect(peer);
        let _ = self.sender.send(packet);
    }

    pub fn allow_connection(&mut self, player_id: i32, addr: IpAddr) {
        if let Some(peer) = self.peer_by_id(player_id) {
            let same_addr = self
                .clients
                .get(&peer)
                .map(|client| client.addr().ip() == addr)
                .unwrap_or(false);

            if !same_addr {
                self.clients.remove(&peer);
                self.players.remove(&player_id);
            }
        }

        self.allowed.insert(addr, player_id);
    }

    pub fn remove_connection(&mut self, player_id: i32, addr: Option<IpAddr>) {
        let peer = self.peer_by_id(player_id);

        if let Some(peer) = peer
            && let Some(client) = self.clients.remove(&peer)
        {
            self.players.remove(&player_id);
            self.allowed.remove(&client.addr().ip());
            let _ = self.sender.send(Packet::Disconnect(client.peer()));
        }

        if let Some(addr) = addr {
            self.allowed.remove(&addr);
        }
    }

    pub fn create_browser(
        &mut self, player_id: i32, browser_id: i32, url: String, hidden: bool, focused: bool,
    ) {
        self.send_packet(
            player_id,
            packets::CreateBrowser {
                browser_id: browser_id as u32,
                url: url.into(),
                hidden,
                focused,
            },
        );
    }

    pub fn destroy_browser(&mut self, player_id: i32, browser_id: i32) {
        self.send_packet(
            player_id,
            packets::DestroyBrowser {
                browser_id: browser_id as u32,
            },
        );
    }

    pub fn hide_browser(&self, player_id: i32, browser_id: i32, hide: bool) {
        self.send_packet(
            player_id,
            packets::HideBrowser {
                browser_id: browser_id as u32,
                hide,
            },
        );
    }

    pub fn focus_browser(&self, player_id: i32, browser_id: i32, focused: bool) {
        self.send_packet(
            player_id,
            packets::FocusBrowser {
                browser_id: browser_id as u32,
                focused,
            },
        );
    }

    pub fn emit_event(&self, player_id: i32, event: &str, arguments: Vec<packets::EventValue>) {
        self.send_packet(
            player_id,
            packets::EmitEvent {
                event_name: event.into(),
                args: None,
                arguments,
            },
        );
    }

    pub fn always_listen_keys(&self, player_id: i32, browser_id: i32, listen: bool) {
        self.send_packet(
            player_id,
            packets::AlwaysListenKeys {
                browser_id: browser_id as u32,
                listen,
            },
        );
    }

    pub fn has_plugin(&self, player_id: i32) -> bool {
        self.peer_by_id(player_id)
            .map(|addr| self.clients.contains_key(&addr))
            .unwrap_or(false)
    }

    pub fn create_external_browser(
        &self, player_id: i32, browser_id: i32, texture: String, url: String, scale: i32,
    ) {
        self.send_packet(
            player_id,
            packets::CreateExternalBrowser {
                browser_id: browser_id as u32,
                url: url.into(),
                texture: texture.into(),
                scale,
            },
        );
    }

    pub fn append_to_object(&self, player_id: i32, browser_id: i32, object_id: i32) {
        self.send_packet(
            player_id,
            packets::AppendToObject {
                browser_id: browser_id as u32,
                object_id,
            },
        );
    }

    pub fn remove_from_object(&self, player_id: i32, browser_id: i32, object_id: i32) {
        self.send_packet(
            player_id,
            packets::RemoveFromObject {
                browser_id: browser_id as u32,
                object_id,
            },
        );
    }

    pub fn toggle_dev_tools(&self, player_id: i32, browser_id: i32, enabled: bool) {
        self.send_packet(
            player_id,
            packets::ToggleDevTools {
                browser_id: browser_id as u32,
                enabled,
            },
        );
    }

    pub fn set_audio_settings(
        &self, player_id: i32, browser_id: u32, max_distance: f32, reference_distance: f32,
    ) {
        self.send_packet(
            player_id,
            packets::SetAudioSettings {
                browser_id,
                max_distance,
                reference_distance,
            },
        );
    }

    pub fn load_url(&self, player_id: i32, browser_id: u32, url: String) {
        self.send_packet(
            player_id,
            packets::LoadUrl {
                browser_id,
                url: url.into(),
            },
        );
    }

    pub fn receiver(&self) -> Receiver<ServerEvent> {
        self.event_rx.clone()
    }

    fn send_packet<'a, T: TryInto<packets::Packet<'a>, Error = quick_protobuf::Error>>(
        &self, player_id: i32, packet: T,
    ) {
        if let Some(addr) = self.peer_by_id(player_id) {
            let Server {
                clients, sender, ..
            } = self;

            if let Some(client) = clients.get(&addr)
                && let Ok(bytes) = try_into_packet(packet)
            {
                let packet = Packet::new(client.peer(), bytes);
                let _ = sender.send(packet);
            }
        }
    }

    fn peer_by_id(&self, player_id: i32) -> Option<PeerId> {
        self.players.get(&player_id).copied()
    }
}
