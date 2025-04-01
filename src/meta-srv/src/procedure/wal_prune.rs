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
use std::sync::Arc;
use std::time::Duration;

use api::v1::meta::MailboxMessage;
use common_error::ext::BoxedError;
use common_meta::distributed_time_constants::MAILBOX_RTT_SECS;
use common_meta::instruction::{Instruction, InstructionReply, SimpleReply};
use common_meta::key::TableMetadataManagerRef;
use common_meta::kv_backend::ResettableKvBackendRef;
use common_meta::lock_key::RemoteWalLock;
use common_meta::peer::Peer;
use common_meta::RegionIdent;
use common_procedure::error::ToJsonSnafu;
use common_procedure::{
    Context as ProcedureContext, Error as ProcedureError, LockKey, Procedure,
    Result as ProcedureResult, Status,
};
use common_telemetry::{debug, warn};
use log_store::kafka::DEFAULT_PARTITION;
use rskafka::client::partition::UnknownTopicHandling;
use rskafka::client::Client;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use store_api::logstore::EntryId;
use store_api::storage::RegionId;

use crate::error::{self, BuildPartitionClientSnafu, DeleteRecordSnafu, TableMetadataManagerSnafu};
use crate::handler::remote_wal_entryid_handler::REMOTE_WAL_LAST_ENTRY_ID_KEY;
use crate::handler::HeartbeatMailbox;
use crate::service::mailbox::{Channel, MailboxRef};
use crate::Result;

type KafkaClientRef = Arc<Client>;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(MAILBOX_RTT_SECS);
const DELETE_RECORDS_TIMEOUT: i32 = 10;

/// The state of WAL pruning.
#[derive(Debug, Serialize, Deserialize)]
pub enum WalPruneState {
    Prepare,
    SendFlushRequest,
    Prune,
}

pub struct Context {
    /// The Kafka client.
    client: KafkaClientRef,
    /// The table metadata manager.
    table_metadata_manager: TableMetadataManagerRef,
    in_memory: ResettableKvBackendRef,
    server_addr: String,
    mailbox: MailboxRef,
}

/// The data of WAL pruning.
#[derive(Serialize, Deserialize)]
pub struct WalPruneData {
    /// The topic name to prune.
    pub topic: String,
    // Threshold to judge if we need to send flush request.
    // None means no need to flush.
    pub threshold: Option<u64>,
    /// The last entry id for each topic.
    pub last_entry_id_to_prune: Option<EntryId>,
    /// The regions needed to be flushed.
    pub regions_to_flush: Option<Vec<RegionId>>,
    /// The state.
    pub state: WalPruneState,
}

/// The procedure to prune WAL.
pub struct WalPruneProcedure {
    pub data: WalPruneData,
    pub context: Context,
}

impl WalPruneProcedure {
    const TYPE_NAME: &'static str = "metasrv-procedure::WalPrune";

    pub fn new(topic: String, threshold: Option<u64>, context: Context) -> Self {
        Self {
            data: WalPruneData {
                topic,
                threshold,
                last_entry_id_to_prune: None,
                regions_to_flush: None,
                state: WalPruneState::Prepare,
            },
            context,
        }
    }

    pub fn from_json(json: &str, context: Context) -> ProcedureResult<Self> {
        let data: WalPruneData = serde_json::from_str(json).context(ToJsonSnafu)?;
        Ok(Self { data, context })
    }

    async fn build_flush_region_instruction(
        &self,
        ctx: &Context,
        region_id: RegionId,
    ) -> Result<Option<(Peer, Instruction)>> {
        let table_id = region_id.table_id();
        let (table_id, table_route) = ctx
            .table_metadata_manager
            .table_route_manager()
            .get_physical_table_route(table_id)
            .await
            .context(error::TableMetadataManagerSnafu)
            .map_err(BoxedError::new)
            .with_context(|_| error::RetryLaterWithSourceSnafu {
                reason: format!("Failed to get TableRoute: {table_id}"),
            })?;

        for region_route in table_route.region_routes {
            if region_route.region.id == region_id {
                if let Some(peer) = region_route.leader_peer {
                    let region_ident = RegionIdent {
                        datanode_id: peer.id,
                        table_id,
                        region_number: region_id.region_number(),
                        // Don't need.
                        engine: "".to_string(),
                    };
                    let instruction = Instruction::FlushRegion(region_ident);
                    return Ok(Some((peer, instruction)));
                }
            }
        }
        Ok(None)
    }

    /// Prepare the last entry id to prune and regions to flush in the WAL.
    ///
    /// Retry:
    /// - Failed to retrieve any metadata.
    pub async fn on_prepare(&mut self) -> Result<Status> {
        let region_ids = self
            .context
            .table_metadata_manager
            .topic_region_manager()
            .regions(&self.data.topic)
            .await
            .context(TableMetadataManagerSnafu)
            .map_err(BoxedError::new)
            .with_context(|_| error::RetryLaterWithSourceSnafu {
                reason: "Failed to get topic-region map",
            })?;
        let kv = self
            .context
            .in_memory
            .get(REMOTE_WAL_LAST_ENTRY_ID_KEY.as_bytes())
            .await
            .context(error::KvBackendSnafu)
            .unwrap_or_default();
        let last_entry_ids_map: HashMap<RegionId, EntryId> = if let Some(kv) = kv {
            serde_json::from_slice(kv.value()).context(error::DeserializeFromJsonSnafu {
                input: String::from_utf8_lossy(kv.value()),
            })?
        } else {
            // Should not happenning since we will submit the procedure after several heartbeat epochs.
            // If it happens, the procedure will do nothing based on the empty map.
            warn!("No remote WAL entry IDs in memory store");
            return Ok(Status::done());
        };

        let mut regions_to_flush = Vec::new();
        // Map last entry id to each topic
        let mut last_entry_id = None;
        // `None` means no region for the topic.
        if region_ids.is_empty() {
            last_entry_id = None;
        }
        let mut min_last_entry_id = 0;
        let mut max_last_entry_id = 0;

        // Find the smallest and largest last entry id.
        for region_id in region_ids.iter() {
            let current_entry_id = last_entry_ids_map.get(region_id).copied();
            if let Some(current_entry_id) = current_entry_id {
                // We should use the `smallest last entry - 1` id to prune.
                min_last_entry_id = min_last_entry_id.min(current_entry_id - 1);
                // Used to judge if we need to flush the region.
                max_last_entry_id = max_last_entry_id.max(current_entry_id);
            }
        }
        // Zero means no need to prune.
        if min_last_entry_id != 0 {
            // Prune at the min last entry id.
            last_entry_id = Some(min_last_entry_id);
        }

        // We need to send flush request to stale regions.
        if let Some(threshold) = self.data.threshold {
            for region_id in region_ids {
                let last_entry_id = last_entry_ids_map.get(&region_id).copied();
                if let Some(last_entry_id) = last_entry_id {
                    if max_last_entry_id - last_entry_id > threshold {
                        regions_to_flush.push(region_id);
                    }
                }
            }
        }

        self.data.last_entry_id_to_prune = last_entry_id;
        self.data.regions_to_flush = Some(regions_to_flush);
        self.data.state = WalPruneState::SendFlushRequest;
        Ok(Status::executing(true))
    }

    pub async fn on_sending_flush_request(&mut self) -> Result<Status> {
        // Safety: regions_to_flush is loaded in on_prepare.
        for region_id in self.data.regions_to_flush.as_ref().unwrap() {
            let flush_instruction = self
                .build_flush_region_instruction(&self.context, *region_id)
                .await?;
            if let Some((peer, flush_instruction)) = flush_instruction {
                let msg = MailboxMessage::json_message(
                    &format!("Flush region: {}", region_id),
                    &format!("Metasrv@{}", self.context.server_addr),
                    &format!("Datanode-{}@{}", peer.id, peer.addr),
                    common_time::util::current_time_millis(),
                    &flush_instruction,
                )
                .with_context(|_| error::SerializeToJsonSnafu {
                    input: flush_instruction.to_string(),
                })?;

                let ch = Channel::Datanode(peer.id);
                let receiver = self.context.mailbox.send(&ch, msg, FLUSH_TIMEOUT).await?;

                // Emit a warning if something goes wrong.
                match receiver.await? {
                    Ok(msg) => {
                        let reply = HeartbeatMailbox::json_reply(&msg)?;
                        let InstructionReply::FlushRegion(SimpleReply { result, error }) = reply
                        else {
                            warn!(
                                "Failed to flush region {}, unexpected reply: {:?}",
                                region_id, reply
                            );
                            self.data.state = WalPruneState::Prune;
                            return Ok(Status::executing(true));
                        };

                        if result {
                            debug!("Flush region {} successfully", region_id);
                        } else {
                            warn!("Failed to flush region {}, error: {:?}", region_id, error);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to flush region {}, error: {:?}", region_id, e);
                    }
                }
            }
        }
        self.data.state = WalPruneState::Prune;
        Ok(Status::executing(true))
    }

    /// Prune the WAL.
    pub async fn on_prune(&mut self) -> Result<Status> {
        // Safety: last_entry_ids are loaded in on_prepare.
        if let Some(last_entry_id_to_prune) = self.data.last_entry_id_to_prune {
            let partition_client = self
                .context
                .client
                .partition_client(
                    self.data.topic.clone(),
                    DEFAULT_PARTITION,
                    UnknownTopicHandling::Retry,
                )
                .await
                .context(BuildPartitionClientSnafu {
                    topic: self.data.topic.clone(),
                    partition: DEFAULT_PARTITION,
                })?;

            partition_client
                .delete_records(last_entry_id_to_prune as i64, DELETE_RECORDS_TIMEOUT)
                .await
                .context(DeleteRecordSnafu {
                    topic: self.data.topic.clone(),
                    partition: DEFAULT_PARTITION,
                    offset: last_entry_id_to_prune,
                })?;
        }
        Ok(Status::done())
    }

    pub async fn rollback_inner(&mut self) -> Result<()> {
        if self.data.regions_to_flush.is_none() {
            self.on_prepare().await?;
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl Procedure for WalPruneProcedure {
    fn type_name(&self) -> &str {
        Self::TYPE_NAME
    }

    fn rollback_supported(&self) -> bool {
        false
    }

    async fn execute(&mut self, _ctx: &ProcedureContext) -> ProcedureResult<Status> {
        let state = &self.data.state;

        match state {
            WalPruneState::Prepare => self.on_prepare().await,
            WalPruneState::SendFlushRequest => self.on_sending_flush_request().await,
            WalPruneState::Prune => self.on_prune().await,
        }
        .map_err(|e| {
            if e.is_retryable() {
                ProcedureError::retry_later(e)
            } else {
                ProcedureError::external(e)
            }
        })
    }

    fn dump(&self) -> ProcedureResult<String> {
        serde_json::to_string(&self.data).context(ToJsonSnafu)
    }

    fn lock_key(&self) -> LockKey {
        let lock_key = vec![RemoteWalLock::Read.into()];
        LockKey::new(lock_key)
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches::assert_matches;

    use common_meta::kv_backend::memory::MemoryKvBackend;
    use common_meta::wal_options_allocator::build_kafka_topic_creator;
    use common_wal::config::kafka::common::{KafkaConnectionConfig, KafkaTopicConfig};
    use common_wal::config::kafka::MetasrvKafkaConfig;
    use common_wal::test_util::run_test_with_kafka_wal;

    use super::*;
    use crate::procedure::region_migration::test_util::TestingEnv;
    use crate::procedure::test_util::new_wal_prune_metadata;

    /// Mock a test env for testing.
    /// Including:
    /// 1. Create a test env with a mailbox, a table metadata manager and a in-memory kv backend.
    /// 2. Prepare some data in the table metadata manager and in-memory kv backend.
    /// 3. Generate a `WalPruneProcedure` with the test env.
    /// 4. Return the test env, the procedure, the minimum last entry id to prune and the regions to flush.
    async fn mock_test_env(
        broker_endpoints: Vec<String>,
    ) -> (TestingEnv, WalPruneProcedure, Option<u64>, Vec<RegionId>) {
        let mut env = TestingEnv::new();
        // Creates a topic manager.
        let kafka_topic = KafkaTopicConfig {
            replication_factor: broker_endpoints.len() as i16,
            ..Default::default()
        };
        let config = MetasrvKafkaConfig {
            connection: KafkaConnectionConfig {
                broker_endpoints,
                ..Default::default()
            },
            kafka_topic,
            ..Default::default()
        };
        let topic_creator = build_kafka_topic_creator(&config).await.unwrap();
        let table_metadata_manager = env.table_metadata_manager().clone();
        let in_memory = Arc::new(MemoryKvBackend::new());
        let mailbox_ctx = env.mailbox_context();

        let topic = "test_topic".to_string();
        let (min_last_entry_id, regions_to_flush) = new_wal_prune_metadata(
            table_metadata_manager.clone(),
            in_memory.clone(),
            10,
            5,
            10,
            topic.clone(),
        )
        .await;

        let context = Context {
            client: topic_creator.client().clone(),
            table_metadata_manager,
            in_memory,
            server_addr: "mock_server_addr".to_string(),
            mailbox: mailbox_ctx.mailbox().clone(),
        };

        let wal_prune_procedure = WalPruneProcedure::new(topic, Some(5), context);
        (
            env,
            wal_prune_procedure,
            min_last_entry_id,
            regions_to_flush,
        )
    }

    fn mock_flush_reply(
        region_id: RegionId,
        result: bool,
        error: Option<String>,
    ) -> MailboxMessage {
        let instruction = InstructionReply::FlushRegion(SimpleReply { result, error });
        MailboxMessage::json_message(
            &format!("Flushed region: {}", region_id),
            &format!(
                "Datanode-{}@{}",
                region_id.table_id(),
                region_id.region_number()
            ),
            &format!("Metasrv@{}", "mock_server_addr"),
            common_time::util::current_time_millis(),
            &instruction,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_procedure_execution() {
        run_test_with_kafka_wal(|broker_endpoints| {
            Box::pin(async {
                common_telemetry::init_default_ut_logging();
                let region_ids = vec![RegionId::new(1, 1), RegionId::new(1, 2)];
                let (mut env, mut procedure, min_last_entry_id, regions_to_flush) =
                    mock_test_env(broker_endpoints).await;

                // Step 1: Test `on_prepare`.
                let status = procedure.on_prepare().await.unwrap();
                assert_matches!(status, Status::Executing { persist: true });
                assert_matches!(procedure.data.state, WalPruneState::SendFlushRequest);
                assert_eq!(procedure.data.last_entry_id_to_prune, min_last_entry_id,);
                assert_eq!(procedure.data.regions_to_flush, Some(regions_to_flush));

                // Step 2: Test `on_sending_flush_request`.
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);

                env.mailbox_context()
                    .insert_heartbeat_response_receiver(Channel::Datanode(1), tx)
                    .await;

                common_runtime::spawn_global(async move {
                    procedure.on_sending_flush_request().await.unwrap();
                });

                for region_id in &region_ids {
                    let resp = rx.recv().await.unwrap().unwrap();
                    let msg = resp.mailbox_message.unwrap();
                    let instruction = HeartbeatMailbox::json_instruction(&msg).unwrap();
                    assert_eq!(
                        instruction,
                        Instruction::FlushRegion(RegionIdent {
                            datanode_id: 1,
                            table_id: region_id.table_id(),
                            region_number: region_id.region_number(),
                            engine: "".to_string()
                        })
                    );
                    let reply = mock_flush_reply(*region_id, true, None);
                    let mailbox = env.mailbox_context().mailbox();
                    mailbox.on_recv(msg.id, Ok(reply)).await.unwrap();
                }
            })
        })
        .await;
    }
}
