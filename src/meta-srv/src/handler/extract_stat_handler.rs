// Copyright 2023 Greptime Team
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

use api::v1::meta::{HeartbeatRequest, Role};
use common_meta::datanode::Stat;
use common_telemetry::{info, warn};

use crate::error::Result;
use crate::handler::{HandleControl, HeartbeatAccumulator, HeartbeatHandler};
use crate::metasrv::Context;
use crate::metrics::{METRIC_META_HEARTBEAT_RATE, METRIC_META_HEARTBEAT_STAT_MEMORY_SIZE};

pub struct ExtractStatHandler;

#[async_trait::async_trait]
impl HeartbeatHandler for ExtractStatHandler {
    fn is_acceptable(&self, role: Role) -> bool {
        role == Role::Datanode
    }

    async fn handle(
        &self,
        req: &HeartbeatRequest,
        _ctx: &mut Context,
        acc: &mut HeartbeatAccumulator,
    ) -> Result<HandleControl> {
        if req.mailbox_message.is_some() {
            // If the heartbeat is a mailbox message, it may have no other valid information,
            // so we don't need to collect stats.
            return Ok(HandleControl::Continue);
        }

        match Stat::try_from(req) {
            Ok(stat) => {
                METRIC_META_HEARTBEAT_RATE.inc();
                METRIC_META_HEARTBEAT_STAT_MEMORY_SIZE.observe(stat.memory_size() as f64);
                let _ = acc.stat.insert(stat);
            }
            Err(Some(header)) => {
                info!("New handshake request: {:?}", header);
            }
            Err(_) => {
                warn!("Incomplete heartbeat data: {:?}", req);
            }
        };

        Ok(HandleControl::Continue)
    }
}
