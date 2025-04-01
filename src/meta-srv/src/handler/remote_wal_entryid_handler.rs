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

use std::collections::HashMap;

use api::v1::meta::{HeartbeatRequest, Role};
use async_trait::async_trait;
use common_meta::datanode::REGION_STATISTIC_LAST_ENTRY_ID_KEY;
use common_meta::kv_backend::ResettableKvBackendRef;
use common_meta::rpc::store::PutRequest;
use common_telemetry::warn;
use snafu::ResultExt;

use crate::error::{DeserializeFromJsonSnafu, Result, SerializeToJsonSnafu};
use crate::handler::{HandleControl, HeartbeatAccumulator, HeartbeatHandler};
use crate::metasrv::Context;

pub const REMOTE_WAL_LAST_ENTRY_ID_KEY: &str = "remote_wal_last_entry_id";

/// Handler for pruning remote WAL entries.
/// Only activated when using remote WAL.
pub struct RemoteWalEntryIdHandler;

#[async_trait]
impl HeartbeatHandler for RemoteWalEntryIdHandler {
    fn is_acceptable(&self, role: Role) -> bool {
        role == Role::Datanode
    }

    async fn handle(
        &self,
        req: &HeartbeatRequest,
        ctx: &mut Context,
        acc: &mut HeartbeatAccumulator,
    ) -> Result<HandleControl> {
        let Some(_) = acc.stat.as_ref() else {
            return Ok(HandleControl::Continue);
        };

        let mut region_entry_ids = HashMap::with_capacity(req.region_stats.len());

        for region_stat in req.region_stats.iter() {
            let region_id = region_stat.region_id;

            if let Some(last_entry_id) = region_stat
                .extensions
                .get(REGION_STATISTIC_LAST_ENTRY_ID_KEY)
            {
                // Safety: Should be a valid u64 from datanode.
                let last_entry_id = String::from_utf8_lossy(last_entry_id)
                    .parse::<u64>()
                    .unwrap();
                region_entry_ids.insert(region_id, last_entry_id);
            }
        }

        update_in_memory_region_last_entry_id(&ctx.in_memory, region_entry_ids).await?;

        Ok(HandleControl::Continue)
    }
}

pub async fn update_in_memory_region_last_entry_id(
    in_memory: &ResettableKvBackendRef,
    region_entry_ids: HashMap<u64, u64>,
) -> Result<()> {
    let kv = in_memory
        .get(REMOTE_WAL_LAST_ENTRY_ID_KEY.as_bytes())
        .await
        .unwrap_or_default();
    let mut in_memory_region_entry_ids: HashMap<u64, u64> = if let Some(kv) = kv {
        serde_json::from_slice(kv.value()).context(DeserializeFromJsonSnafu {
            input: String::from_utf8_lossy(kv.value()),
        })?
    } else {
        warn!("No remote WAL entry IDs cached in memory store");
        HashMap::new()
    };

    in_memory_region_entry_ids.extend(region_entry_ids);
    let put_req = PutRequest {
        key: REMOTE_WAL_LAST_ENTRY_ID_KEY.as_bytes().to_vec(),
        value: serde_json::to_vec(&in_memory_region_entry_ids).context(SerializeToJsonSnafu {
            input: format!("{:?}", in_memory_region_entry_ids),
        })?,
        ..Default::default()
    };
    let put_resp = in_memory.put(put_req).await;

    if let Err(err) = put_resp {
        warn!("Failed to put remote WAL entry IDs into memory store: {err}");
    }

    Ok(())
}
