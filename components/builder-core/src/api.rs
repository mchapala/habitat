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

use hab_net::routing::Broker;
use protocol::originsrv::{Origin, OriginChannel, OriginChannelCreate, OriginGet};
use protocol::net::ErrCode;

use error::{Error, Result};

pub fn create_channel(origin: &str, channel: &str, session_id: u64) -> Result<(OriginChannel)> {
    let origin_id = match get_origin(origin)? {
        Some(o) => o.get_id(),
        None => {
            debug!("Origin {} not found!", origin);
            return Err(Error::OriginNotFound(origin.to_string()));
        }
    };

    let mut conn = Broker::connect().unwrap();
    let mut request = OriginChannelCreate::new();

    request.set_owner_id(session_id);
    request.set_origin_name(origin.to_string());
    request.set_origin_id(origin_id);
    request.set_name(channel.to_string());

    match conn.route::<OriginChannelCreate, OriginChannel>(&request) {
        Ok(origin_channel) => Ok(origin_channel),
        Err(err) => Err(Error::NetError(err)),
    }
}

pub fn get_origin<T: ToString>(origin: T) -> Result<Option<Origin>> {
    let mut conn = Broker::connect().unwrap();
    let mut request = OriginGet::new();
    request.set_name(origin.to_string());

    match conn.route::<OriginGet, Origin>(&request) {
        Ok(origin) => Ok(Some(origin)),
        Err(err) => {
            if err.get_code() == ErrCode::ENTITY_NOT_FOUND {
                Ok(None)
            } else {
                Err(Error::NetError(err))
            }
        }
    }
}
