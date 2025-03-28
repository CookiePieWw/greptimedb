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

use common_meta::instruction::{InstructionReply, SimpleReply};
use common_meta::RegionIdent;
use common_telemetry::warn;
use futures_util::future::BoxFuture;
use store_api::region_request::{RegionFlushRequest, RegionRequest};

use crate::error;
use crate::heartbeat::handler::HandlerContext;

impl HandlerContext {
    pub(crate) fn handle_flush_region_instruction(
        self,
        region_ident: RegionIdent,
    ) -> BoxFuture<'static, InstructionReply> {
        Box::pin(async move {
            let region_id = Self::region_ident_to_region_id(&region_ident);
            let request = RegionRequest::Flush(RegionFlushRequest {
                row_group_size: None,
            });
            let result = self.region_server.handle_request(region_id, request).await;

            match result {
                Ok(_) => InstructionReply::FlushRegion(SimpleReply {
                    result: true,
                    error: None,
                }),
                Err(error::Error::RegionNotFound { .. }) => {
                    warn!("Received a close region instruction from meta, but target region:{region_id} is not found.");
                    InstructionReply::CloseRegion(SimpleReply {
                        result: true,
                        error: None,
                    })
                }
                Err(err) => InstructionReply::FlushRegion(SimpleReply {
                    result: false,
                    error: Some(format!("{err:?}")),
                }),
            }
        })
    }
}
