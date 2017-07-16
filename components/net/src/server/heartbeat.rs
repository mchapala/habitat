// Copyright (c) 2017 Chef Software Inc. and/or applicable contributors
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

use std::sync::{mpsc, Arc, RwLock};
use std::time::Duration;
use std::thread::{self, JoinHandle};

use protobuf::Message;
use protocol;
use zmq;

use super::{NetIdent, ZMQ_CONTEXT};
use config::{RouterCfg, Shards};
use error::Result;

/// Polling timeout for HeartbeatMgr
const HEARTBEAT_MS: i64 = 30_000;

/// Maintains a presence notification of an application server to another server
pub struct HeartbeatMgr(Connector);

impl HeartbeatMgr {
    /// Start the HeartbeatMgr
    pub fn start(netid: String) -> Result<JoinHandle<()>> {
        let connector = Connector::new(netid);
        let (tx, rx) = mpsc::sync_channel(0);
        let handle = thread::Builder::new()
            .name("heartbeat".to_string())
            .spawn(move || heartbeat.run(tx, addrs))
            .unwrap();
        match rx.recv() {
            Ok(()) => Ok(handle),
            Err(e) => panic!("heartbeat thread startup error, err={}", e),
        }
    }

    // Main loop for server
    fn run(&mut self, rz: mpsc::SyncSender<()>, addrs: Vec<String>) {
        self.0.connect(addrs);
        rz.send(()).unwrap();
        loop {
            self.0.pulse();
            trace!("heartbeat pulsed");
            // JW TODO: diff the time so we don't sleep X plus exec time
            thread::sleep_ms(HEARTBEAT_MS as u32);
        }
    }
}

struct Connector {
    socket: zmq::Socket,
    netid: String,
}

impl Connector {
    fn new(netid: String) -> Self {
        let sock = (**ZMQ_CONTEXT).as_mut().socket(zmq::PUB).unwrap();
        sock.set_immediate(true).unwrap();
        sock.set_sndhwm(1).unwrap();
        sock.set_linger(0).unwrap();
        Connector {
            netid: netid,
            socket: sock,
        }
    }

    fn connect(&self, addrs: Vec<String>) {
        for addr in addrs {
            self.socket.connect(&addr).unwrap();
            info!("Heartbeat socket connected to, {}", addr);
        }
        // This hacky sleep is recommended and required by zmq for connections to establish
        thread::sleep(Duration::from_millis(100));
    }

    // Pulse server presence to connected servers
    fn pulse(&mut self) {
        self.socket.send_str("P", zmq::SNDMORE).unwrap();
        self.socket.send_str(&self.netid, 0).unwrap();
    }
}
