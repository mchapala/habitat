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

use api_client;
use common::ui::{Status, UI};

use {PRODUCT, VERSION};
use error::{Error, Result};

pub fn start(
    ui: &mut UI,
    depot_url: &str,
    group_id: &str,
    channel: &str,
    token: &str,
) -> Result<()> {
    let api_client = api_client::Client::new(depot_url, PRODUCT, VERSION, None)
        .map_err(Error::APIClient)?;
    let gid = match group_id.parse::<u64>() {
        Ok(g) => g,
        Err(e) => {
            ui.fatal(format!("Failed to parse group id: {}", e))?;
            return Err(Error::ParseIntError(e));
        }
    };

    ui.status(
        Status::Promoting,
        format!("job group {} to channel {}", group_id, channel),
    )?;
    api_client.job_group_promote(gid, channel, token).map_err(
        Error::APIClient,
    )?;
    ui.status(
        Status::Promoted,
        format!("job group {} to channel {}", group_id, channel),
    )?;
    Ok(())
}
