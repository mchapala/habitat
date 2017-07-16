// Copyright (c) 2016-2017 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hab_net;
use hab_net::server::{Application, Envelope, ZMQ_CONTEXT};
use protobuf::{parse_from_bytes, Message};
use protocol::{self, routesrv};
use protocol::sharding::{ShardId, SHARD_COUNT};
use protocol::net::{ErrCode, Protocol};
use rand::{self, Rng};
use zmq;

use config::Config;
use error::{Error, Result};

#[derive(Default)]
struct ServerMap(HashMap<Protocol, HashMap<ShardId, hab_net::ServerReg>>);

impl ServerMap {
    pub fn add(&mut self, protocol: Protocol, netid: String, shards: Vec<ShardId>) -> Result<()> {
        if !self.servers.contains_key(&protocol) {
            self.servers.insert(protocol, HashMap::default());
        }
        let shards = self.servers.get_mut(&protocol).unwrap();
        for shard in shards {
            // JW TODO: Check if we think we already have a server servicing this shard
            shards.insert(shard, hab_net::ServerReg::new(netid));
        }
        Ok(())
    }

    pub fn drop(&mut self, protocol: Protocol, netid: &str) {
        if let Some(map) = self.0.get_mut(protocol) {
            map.iter_mut().filter(|reg| reg.endpoint == netid).collect();
        }
    }

    pub fn renew(&mut self, netid: &str) {
        // JW TODO: We can't iterate like this every heartbeat
        for registrations in self.servers.values_mut() {
            for registration in registrations.values_mut() {
                if registration.endpoint == netid {
                    registration.renew();
                }
            }
        }
    }
}

pub struct Server {
    config: Arc<Mutex<Config>>,
    fe_sock: zmq::Socket,
    hb_sock: zmq::Socket,
    servers: ServerMap,
    state: SocketState,
    envelope: Envelope,
    req: zmq::Message,
    rng: rand::ThreadRng,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let fe_sock = (**ZMQ_CONTEXT).as_mut().socket(zmq::ROUTER).unwrap();
        let hb_sock = (**ZMQ_CONTEXT).as_mut().socket(zmq::ROUTER).unwrap();
        fe_sock.set_router_mandatory(true).unwrap();
        hb_sock.set_router_mandatory(true).unwrap();
        Server {
            config: Arc::new(Mutex::new(config)),
            fe_sock: fe_sock,
            hb_sock: hb_sock,
            servers: ServerMap::default(),
            state: SocketState::default(),
            envelope: Envelope::default(),
            req: zmq::Message::new().unwrap(),
            rng: rand::thread_rng(),
        }
    }

    pub fn reconfigure(&self, config: Config) -> Result<()> {
        {
            let mut cfg = self.config.lock().unwrap();
            *cfg = config;
        }
        // obtain lock and replace our config
        // notify datastore to refresh it's connection if it needs to
        // notify sockets to reconnect if changes
        Ok(())
    }

    fn process_frontend(&mut self) -> Result<()> {
        loop {
            match self.state {
                SocketState::Ready => {
                    if self.envelope.max_hops() {
                        // We should force the sender to disconnect, they have a problem.
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    let hop = self.fe_sock.recv_msg(0)?;
                    if self.envelope.hops().len() == 0 && hop.len() == 0 {
                        warn!("rejecting message, failed to receive identity frame from message");
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    debug!("received hop, {:?}/{:?}", hop.as_str(), hop.len());
                    self.envelope.add_hop(hop).unwrap();
                    self.state = SocketState::Hops;
                }
                SocketState::Hops => {
                    if self.envelope.max_hops() {
                        // We should force the sender to disconnect, they have a problem.
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    let hop = self.fe_sock.recv_msg(0)?;
                    if hop.len() == 0 {
                        self.state = SocketState::Control;
                        continue;
                    }
                    debug!("received hop, {:?}/{:?}", hop.as_str(), hop.len());
                    self.envelope.add_hop(hop).unwrap();
                }
                SocketState::Control => {
                    self.fe_sock.recv(&mut self.req, 0)?;
                    match self.req.as_str() {
                        Some("RP") => self.state = SocketState::Forwarding,
                        Some("RQ") => self.state = SocketState::Routing,
                        _ => {
                            warn!("framing error");
                            self.state = SocketState::Cleaning;
                        }
                    }
                }
                SocketState::Forwarding => {
                    self.fe_sock.recv(&mut self.req, 0)?;
                    debug!("forwarding, msg={:?}", self.req.as_str());
                    for hop in &self.envelope.hops()[1..] {
                        self.fe_sock.send(&*hop, zmq::SNDMORE).unwrap();
                    }
                    self.fe_sock.send(&[], zmq::SNDMORE).unwrap();
                    self.fe_sock.send(&*self.req, 0).unwrap();
                    self.state = SocketState::Cleaning;
                }
                SocketState::Routing => {
                    self.fe_sock.recv(&mut self.req, 0)?;
                    if self.req.len() == 0 {
                        warn!("rejecting message, failed to receive a message body");
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    debug!("received req, {:?}/{:?}", self.req.as_str(), self.req.len());
                    match parse_from_bytes(&self.req) {
                        Ok(msg) => self.envelope.msg = msg,
                        Err(e) => {
                            println!("failed to parse message, err={:?}", e);
                            self.state = SocketState::Cleaning;
                            continue;
                        }
                    }
                    if !self.envelope.msg.has_route_info() {
                        warn!(
                            "received message without route-info, msg={:?}",
                            self.envelope.msg
                        );
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    match self.envelope.msg.get_route_info().get_protocol() {
                        Protocol::RouteSrv => self.handle_message()?,
                        _ => self.route_message()?,
                    }
                    self.state = SocketState::Cleaning;
                }
                SocketState::Cleaning => {
                    debug!("cleaning socket state");
                    self.reset();
                    self.state = SocketState::Ready;
                    break;
                }
            }
        }
        Ok(())
    }

    fn process_heartbeat(&mut self) -> Result<()> {
        self.hb_sock.recv(&mut self.req, 0)?;
        match self.req.as_str() {
            Some("") | None => return Ok(()),
            Some("R") => {
                // Registration
                self.hb_sock.recv(&mut self.req, 0)?;
                let reg: routesrv::Registration = parse_from_bytes(&self.req)?;
                debug!("received server reg, {:?}", registration);
                match self.servers.add(
                    reg.get_protocol(),
                    reg.take_endpoint(),
                    reg.take_shards(),
                ) {
                    Ok(()) => self.hb_sock.send_str("REGOK", 0),
                    Err(_) => self.hb_sock.send_str("REGCONFLICT", 0),
                }
            }
            Some("P") => {
                // Pulse
                self.hb_sock.recv(&mut self.req, 0)?;
                self.servers.renew(self.req.as_str());
            }
            Some(ident) => {
                // New connection
                self.hb_sock.send_str(ident, zmq::SNDMORE)?;
                self.hb_sock.send(&[], zmq::SNDMORE)?;
                self.hb_sock.send_str("REG", 0)?;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.envelope.reset();
    }

    fn handle_message(&mut self) -> Result<()> {
        let msg = &self.envelope.msg;
        trace!("handle-message, msg={:?}", &msg);
        match self.envelope.message_id() {
            "Connect" => {
                let req: routesrv::Connect = parse_from_bytes(msg.get_body()).unwrap();
                trace!("Connect={:?}", req);
                let rep = protocol::Message::new(&routesrv::ConnectOk::new()).build();
                self.fe_sock
                    .send(&rep.write_to_bytes().unwrap(), 0)
                    .unwrap();
            }
            "Disconnect" => {
                let req: routesrv::Disconnect = parse_from_bytes(msg.get_body()).unwrap();
                trace!("Disconnect={:?}", req);
                self.servers.drop(req.get_protocol(), req.get_endpoint());
            }
            id => warn!("Unknown message, msg={}", id),
        }
        Ok(())
    }

    fn route_message(&mut self) -> Result<()> {
        let shard = self.select_shard();
        match self.servers.get(&self.envelope.protocol()) {
            Some(shards) => {
                match shards.get(&shard) {
                    Some(server) => {
                        debug!(
                            "routing, srv={:?}, hops={:?}, msg={:?}",
                            server.endpoint,
                            self.envelope.hops().len(),
                            self.envelope.msg
                        );
                        self.fe_sock.send_str(&server.endpoint, zmq::SNDMORE)?;
                        for hop in self.envelope.hops() {
                            self.fe_sock.send(&*hop, zmq::SNDMORE)?;
                        }
                        self.fe_sock.send(&[], zmq::SNDMORE)?;
                        self.fe_sock.send(
                            &self.envelope.msg.write_to_bytes().unwrap(),
                            0,
                        )?;
                    }
                    None => {
                        warn!(
                            "failed to route message, no server servicing shard, msg={:?}",
                            self.envelope.msg
                        );
                        let err = protocol::Message::new(
                            &protocol::net::err(ErrCode::NO_SHARD, "rt:route:1"),
                        ).build();
                        let bytes = err.write_to_bytes()?;
                        for hop in self.envelope.hops() {
                            self.fe_sock.send(&*hop, zmq::SNDMORE)?;
                        }
                        self.fe_sock.send(&[], zmq::SNDMORE)?;
                        self.fe_sock.send(&bytes, 0)?;
                    }
                }
            }
            None => {
                warn!(
                    "failed to route message, no servers registered for protocol, msg={:?}",
                    self.envelope.msg
                );
                let err = protocol::Message::new(
                    &protocol::net::err(ErrCode::NO_SHARD, "rt:route:2"),
                ).build();
                let bytes = err.write_to_bytes()?;
                for hop in self.envelope.hops() {
                    self.fe_sock.send(&*hop, zmq::SNDMORE)?;
                }
                self.fe_sock.send(&[], zmq::SNDMORE)?;
                self.fe_sock.send(&bytes, 0)?;
            }
        }
        Ok(())
    }

    fn select_shard(&mut self) -> u32 {
        if self.envelope.route_info().has_hash() {
            (self.envelope.route_info().get_hash() % SHARD_COUNT as u64) as u32
        } else {
            (self.rng.gen::<u64>() % SHARD_COUNT as u64) as u32
        }
    }
}

impl Application for Server {
    type Error = Error;

    fn run(&mut self) -> Result<()> {
        {
            let cfg = self.config.lock().unwrap();
            self.hb_sock.bind(&cfg.hb_addr())?;
            self.fe_sock.bind(&cfg.fe_addr())?;
            println!("Listening on ({})", cfg.fe_addr());
            println!("Heartbeat on ({})", cfg.hb_addr());
        }
        info!("builder-router is ready to go.");
        let mut hb_msg = false;
        let mut fe_msg = false;
        loop {
            {
                let mut items = [
                    self.hb_sock.as_poll_item(zmq::POLLIN),
                    self.fe_sock.as_poll_item(zmq::POLLIN),
                ];
                trace!("waiting for message");
                // JW TODO: sleep for duration of next heartbeat we need to expire. Sleep forever
                // if we haven't heard from anyone yet.
                zmq::poll(&mut items, -1)?;
                if items[0].is_readable() {
                    hb_msg = true;
                }
                if items[1].is_readable() {
                    fe_msg = true;
                }
            }
            if hb_msg {
                trace!("processing heartbeat");
                self.process_heartbeat()?;
            }
            if fe_msg {
                trace!("processing front-end");
                self.process_frontend()?;
            }
            hb_msg = false;
            fe_msg = false;
            trace!("done processing");
        }
    }
}

enum SocketState {
    Ready,
    Hops,
    Control,
    Routing,
    Forwarding,
    Cleaning,
}

impl Default for SocketState {
    fn default() -> SocketState {
        SocketState::Ready
    }
}

pub fn run(config: Config) -> Result<()> {
    Server::new(config).run()
}
