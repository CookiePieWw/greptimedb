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

use api::v1::meta::mailbox_message::Payload;
use api::v1::meta::{HeartbeatResponse, MailboxMessage};
use common_meta::instruction::{
    DowngradeRegionReply, InstructionReply, SimpleReply, UpgradeRegionReply,
};
use common_meta::key::table_route::TableRouteValue;
use common_meta::key::test_utils::new_test_table_info;
use common_meta::key::TableMetadataManagerRef;
use common_meta::kv_backend::ResettableKvBackendRef;
use common_meta::peer::Peer;
use common_meta::rpc::router::{Region, RegionRoute};
use common_meta::sequence::Sequence;
use common_time::util::current_time_millis;
use store_api::storage::RegionId;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::error::Result;
use crate::handler::remote_wal_entryid_handler::update_in_memory_region_last_entry_id;
use crate::handler::{HeartbeatMailbox, Pusher, Pushers};
use crate::service::mailbox::{Channel, MailboxRef};

pub type MockHeartbeatReceiver = Receiver<std::result::Result<HeartbeatResponse, tonic::Status>>;

/// The context of mailbox.
pub struct MailboxContext {
    mailbox: MailboxRef,
    // The pusher is used in the mailbox.
    pushers: Pushers,
}

impl MailboxContext {
    pub fn new(sequence: Sequence) -> Self {
        let pushers = Pushers::default();
        let mailbox = HeartbeatMailbox::create(pushers.clone(), sequence);

        Self { mailbox, pushers }
    }

    /// Inserts a pusher for `datanode_id`
    pub async fn insert_heartbeat_response_receiver(
        &mut self,
        channel: Channel,
        tx: Sender<std::result::Result<HeartbeatResponse, tonic::Status>>,
    ) {
        let pusher_id = channel.pusher_id();
        let pusher = Pusher::new(tx);
        let _ = self.pushers.insert(pusher_id.string_key(), pusher).await;
    }

    pub fn mailbox(&self) -> &MailboxRef {
        &self.mailbox
    }
}

/// Sends a mock reply.
pub fn send_mock_reply(
    mailbox: MailboxRef,
    mut rx: MockHeartbeatReceiver,
    msg: impl Fn(u64) -> Result<MailboxMessage> + Send + 'static,
) {
    common_runtime::spawn_global(async move {
        while let Some(Ok(resp)) = rx.recv().await {
            let reply_id = resp.mailbox_message.unwrap().id;
            mailbox.on_recv(reply_id, msg(reply_id)).await.unwrap();
        }
    });
}

/// Generates a [InstructionReply::OpenRegion] reply.
pub(crate) fn new_open_region_reply(
    id: u64,
    result: bool,
    error: Option<String>,
) -> MailboxMessage {
    MailboxMessage {
        id,
        subject: "mock".to_string(),
        from: "datanode".to_string(),
        to: "meta".to_string(),
        timestamp_millis: current_time_millis(),
        payload: Some(Payload::Json(
            serde_json::to_string(&InstructionReply::OpenRegion(SimpleReply { result, error }))
                .unwrap(),
        )),
    }
}

/// Generates a [InstructionReply::CloseRegion] reply.
pub fn new_close_region_reply(id: u64) -> MailboxMessage {
    MailboxMessage {
        id,
        subject: "mock".to_string(),
        from: "datanode".to_string(),
        to: "meta".to_string(),
        timestamp_millis: current_time_millis(),
        payload: Some(Payload::Json(
            serde_json::to_string(&InstructionReply::CloseRegion(SimpleReply {
                result: false,
                error: None,
            }))
            .unwrap(),
        )),
    }
}

/// Generates a [InstructionReply::DowngradeRegion] reply.
pub fn new_downgrade_region_reply(
    id: u64,
    last_entry_id: Option<u64>,
    exist: bool,
    error: Option<String>,
) -> MailboxMessage {
    MailboxMessage {
        id,
        subject: "mock".to_string(),
        from: "datanode".to_string(),
        to: "meta".to_string(),
        timestamp_millis: current_time_millis(),
        payload: Some(Payload::Json(
            serde_json::to_string(&InstructionReply::DowngradeRegion(DowngradeRegionReply {
                last_entry_id,
                exists: exist,
                error,
            }))
            .unwrap(),
        )),
    }
}

/// Generates a [InstructionReply::UpgradeRegion] reply.
pub fn new_upgrade_region_reply(
    id: u64,
    ready: bool,
    exists: bool,
    error: Option<String>,
) -> MailboxMessage {
    MailboxMessage {
        id,
        subject: "mock".to_string(),
        from: "datanode".to_string(),
        to: "meta".to_string(),
        timestamp_millis: current_time_millis(),
        payload: Some(Payload::Json(
            serde_json::to_string(&InstructionReply::UpgradeRegion(UpgradeRegionReply {
                ready,
                exists,
                error,
            }))
            .unwrap(),
        )),
    }
}

pub async fn new_wal_prune_metadata(
    table_metadata_manager: TableMetadataManagerRef,
    in_memory: ResettableKvBackendRef,
    region_n: u32,
    table_n: u32,
    threshold: u64,
    topic: String,
) -> (Option<u64>, Vec<RegionId>) {
    let from_peer = Peer::empty(1);
    let mut min_last_entry_id = 0;
    let mut region_entry_ids = HashMap::with_capacity(table_n as usize * region_n as usize);
    for table_id in 0..table_n {
        let region_ids = (0..region_n)
            .map(|i| RegionId::new(table_id, i))
            .collect::<Vec<_>>();
        let table_info = new_test_table_info(table_id, 0..region_n).into();
        let region_routes = region_ids
            .iter()
            .map(|region_id| RegionRoute {
                region: Region::new_test(*region_id),
                leader_peer: Some(from_peer.clone()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let region_wal_options: HashMap<u32, String> = (0..region_n)
            .map(|region_number| (region_number, topic.clone()))
            .collect();

        table_metadata_manager
            .create_table_metadata(
                table_info,
                TableRouteValue::physical(region_routes),
                region_wal_options.clone(),
            )
            .await
            .unwrap();

        let current_region_entry_ids = region_ids
            .iter()
            .map(|region_id| {
                let current_last_entry_id = rand::random::<u64>();
                min_last_entry_id = min_last_entry_id.min(current_last_entry_id);
                (region_id.as_u64(), current_last_entry_id)
            })
            .collect::<HashMap<_, _>>();
        region_entry_ids.extend(current_region_entry_ids.clone());
        update_in_memory_region_last_entry_id(&in_memory, current_region_entry_ids)
            .await
            .unwrap();
    }

    let regions_to_flush = region_entry_ids
        .iter()
        .filter_map(|(region_id, last_entry_id)| {
            if last_entry_id - min_last_entry_id > threshold {
                Some(RegionId::from_u64(*region_id))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let min_last_entry_id = if min_last_entry_id == 0 {
        None
    } else {
        Some(min_last_entry_id)
    };
    (min_last_entry_id, regions_to_flush)
}
