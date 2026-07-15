use alloy::primitives::ruint::FromUintError;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::collections::HashMap;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::InteropRoot;
use zksync_os_provider::NodeProvider;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1WatcherError, StartResolver};
use crate::{EventSink, L1WatcherConfig, ProcessRawEvents};

/// Watches `NewInteropRoot` updates emitted by L1's MessageRoot and feeds them into the interop
/// subpool.
///
/// After gateway settlement-layer support was removed every chain settles on L1, so the interop
/// roots a chain must import are the `NewInteropRoot` events L1's MessageRoot emits (era-contracts
/// `MessageRootBase.addChainBatchRoot` builds the interop tree on L1). This is a single-segment L1
/// watcher over that MessageRoot address; the `interop_root_id` cursor resolves to the first L1
/// block to scan. It de-duplicates multiple logs for the same `logId` and inserts the latest
/// `IndexedInteropRoot` into its sink.
pub struct InteropWatcher {
    starting_interop_root_id: u64,
    sink: Box<dyn EventSink<IndexedInteropRoot>>,
}

impl InteropWatcher {
    /// Builds the L1 interop-roots watcher. The events come from L1's MessageRoot (resolved from
    /// the L1 bridgehub); the start block is derived from the chain's `interop_root_id` cursor.
    pub async fn create_watcher(
        config: L1WatcherConfig,
        l1_bridgehub: Bridgehub<NodeProvider>,
        sink: impl EventSink<IndexedInteropRoot>,
    ) -> anyhow::Result<StartResolver<u64, Self>> {
        let provider = l1_bridgehub.provider().clone();
        let message_root = l1_bridgehub
            .message_root_address()
            .await
            .context("failed to fetch L1 message_root address for interop watcher")?;

        let resolve_start = move |starting_interop_root_id: u64| async move {
            let start_block =
                find_l1_block_by_interop_root_id(l1_bridgehub.clone(), starting_interop_root_id)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to find L1 block for interop_root_id={starting_interop_root_id}"
                        )
                    })?;
            let processor = Self {
                starting_interop_root_id,
                sink: Box::new(sink),
            };
            Ok((start_block, processor))
        };

        Ok(StartResolver::new(
            config,
            provider,
            message_root.into(),
            None,
            resolve_start,
        ))
    }
}

#[async_trait::async_trait]
impl ProcessRawEvents for InteropWatcher {
    fn name(&self) -> &'static str {
        "interop_root"
    }

    fn event_signatures(&self) -> Topic {
        NewInteropRoot::SIGNATURE_HASH.into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        // we want to accept only the latest event for each log id
        let mut indexes = HashMap::new();

        for log in logs {
            let event = match NewInteropRoot::decode_log(&log.inner) {
                Ok(event) => event.data,
                Err(err) => {
                    tracing::error!(?log, error = ?err, "failed to decode interop root log");
                    continue;
                }
            };
            indexes.insert(event.logId, log);
        }

        indexes.into_values().collect()
    }

    async fn process_raw_event(
        &mut self,
        _provider: &NodeProvider,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let event = NewInteropRoot::decode_log(&log.inner)?.data;

        let log_id: u64 = event
            .logId
            .try_into()
            .map_err(|e: FromUintError<u64>| L1WatcherError::Other(e.into()))?;

        if log_id < self.starting_interop_root_id {
            tracing::debug!(
                log_id,
                starting_interop_root_id = self.starting_interop_root_id,
                "skipping interop root event before starting id",
            );
            return Ok(());
        }
        let interop_root = InteropRoot {
            chainId: event.chainId,
            blockOrBatchNumber: event.blockNumber,
            sides: event.sides.clone(),
        };

        self.sink
            .push(IndexedInteropRoot {
                log_id,
                root: interop_root,
            })
            .await;
        Ok(())
    }
}
