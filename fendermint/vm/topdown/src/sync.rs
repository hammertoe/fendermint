// Copyright 2022-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT
//! A constant running process that fetch or listener to parent state

use crate::error::Error;
use crate::{
    BlockHash, BlockHeight, CachedFinalityProvider, Config, IPCParentFinality,
    ParentFinalityProvider, Toggle,
};
use anyhow::{anyhow, Context};
use async_stm::{atomically, atomically_or_err};
use fvm_shared::clock::ChainEpoch;
use ipc_agent_sdk::apis::IpcAgentClient;
use ipc_agent_sdk::jsonrpc::JsonRpcClientImpl;
use ipc_agent_sdk::message::ipc::ValidatorSet;
use ipc_sdk::cross::CrossMsg;
use ipc_sdk::subnet_id::SubnetID;
use std::cmp::min;
use std::sync::Arc;
use std::time::Duration;

/// The max number of blocks polling should query each parent view update. If the number of blocks
/// polled equals this value, it would stop polling for this iteration and commit the result to cache.
const MAX_PARENT_VIEW_BLOCK_GAP: BlockHeight = 100;
/// When polling parent view, if the number of top down messages exceeds this limit,
/// the polling will stop for this iteration and commit the result to cache.
const TOPDOWN_MSG_LEN_THRESHOLD: usize = 500;

/// Query the parent finality from the block chain state
pub trait ParentFinalityStateQuery {
    /// Get the latest committed finality from the state
    fn get_latest_committed_finality(&self) -> anyhow::Result<Option<IPCParentFinality>>;
}

/// Constantly syncing with parent through polling
pub struct PollingParentSyncer<T> {
    config: Config,
    parent_view_provider: Arc<Toggle<CachedFinalityProvider>>,
    agent: Arc<IPCAgentProxy>,
    committed_state_query: Arc<T>,
}

/// Queries the starting finality for polling. First checks the committed finality, if none, that
/// means the chain has just started, then query from the parent to get the genesis epoch.
async fn query_starting_finality<T: ParentFinalityStateQuery + Send + Sync + 'static>(
    query: &Arc<T>,
    agent: &Arc<IPCAgentProxy>,
) -> anyhow::Result<IPCParentFinality> {
    loop {
        let mut finality = match query.get_latest_committed_finality() {
            Ok(Some(finality)) => finality,
            Ok(None) => {
                tracing::debug!("app not ready for query yet");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("cannot get committed finality: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        tracing::info!("latest finality committed: {finality:?}");

        // this means there are no previous committed finality yet, we fetch from parent to get
        // the genesis epoch of the current subnet and its corresponding block hash.
        if finality.height == 0 {
            let genesis_epoch = agent.get_genesis_epoch().await?;
            let block_hash = agent.get_block_hash(genesis_epoch as u64).await?;
            finality = IPCParentFinality {
                height: genesis_epoch as u64,
                block_hash,
            };
            tracing::info!(
                "no previous finality committed, fetched from genesis epoch: {finality:?}"
            );
        }

        return Ok(finality);
    }
}

/// Start the polling parent syncer in the background
pub async fn launch_polling_syncer<T: ParentFinalityStateQuery + Send + Sync + 'static>(
    query: T,
    config: Config,
    view_provider: Arc<Toggle<CachedFinalityProvider>>,
    agent: Arc<IPCAgentProxy>,
) -> anyhow::Result<()> {
    if !view_provider.is_enabled() {
        return Err(anyhow!("provider not enabled, enable to run syncer"));
    }

    let query = Arc::new(query);

    let finality = query_starting_finality(&query, &agent).await?;

    atomically(|| view_provider.set_new_finality(finality.clone())).await;

    let poll = PollingParentSyncer::new(config, view_provider, agent, query);
    poll.start();

    Ok(())
}

impl<T> PollingParentSyncer<T> {
    pub fn new(
        config: Config,
        parent_view_provider: Arc<Toggle<CachedFinalityProvider>>,
        agent: Arc<IPCAgentProxy>,
        query: Arc<T>,
    ) -> Self {
        Self {
            config,
            parent_view_provider,
            agent,
            committed_state_query: query,
        }
    }
}

impl<T: ParentFinalityStateQuery + Send + Sync + 'static> PollingParentSyncer<T> {
    /// Start the parent finality listener in the background
    pub fn start(self) {
        let config = self.config;
        let provider = self.parent_view_provider;
        let agent = self.agent;
        let query = self.committed_state_query;

        let mut interval =
            tokio::time::interval(Duration::from_secs(config.polling_interval_secs));

        tokio::spawn(async move {
            loop {
                interval.tick().await;

                if let Err(e) =
                    sync_with_parent(&config, &agent, &provider, &query).await
                {
                    tracing::error!("sync with parent encountered error: {e}");
                }
            }
        });
    }
}

/// Syncing with parent with the below steps:
/// 1. Get the latest height in cache or latest height committed increment by 1 as the
///    starting height
/// 2. Get the latest chain head height deduct away N blocks as the ending height
/// 3. Fetches the data between starting and ending height
/// 4. Update the data into cache
async fn sync_with_parent<T: ParentFinalityStateQuery + Send + Sync + 'static>(
    config: &Config,
    agent_proxy: &Arc<IPCAgentProxy>,
    provider: &Arc<Toggle<CachedFinalityProvider>>,
    query: &Arc<T>,
) -> anyhow::Result<()> {
    let last_recorded_height = if let Some(h) = last_recorded_height(provider).await? {
        h
    } else {
        // cannot get starting recorded height, we just wait for the next loop execution
        return Ok(());
    };

    let parent_chain_head_height = agent_proxy
        .get_chain_head_height()
        .await
        .context("cannot fetch parent chain head")?;
    // sanity check
    if parent_chain_head_height < config.chain_head_delay {
        tracing::debug!("latest height not more than the chain head delay");
        return Ok(());
    }

    let ending_height = parent_chain_head_height - config.chain_head_delay;

    tracing::debug!(
        "last recorded height: {}, parent chain head: {}, ending_height: {}",
        last_recorded_height,
        parent_chain_head_height,
        ending_height
    );

    if last_recorded_height == ending_height {
        tracing::debug!(
            "the parent has yet to produce a new block, stops at height: {last_recorded_height}"
        );
        return Ok(());
    }

    // we are going backwards in terms of block height, the latest block height is lower
    // than our previously fetched head. It could be a chain reorg. We clear all the cache
    // in `provider` and start from scratch
    if last_recorded_height > ending_height {
        let finality = query_starting_finality(query, agent_proxy).await?;
        atomically(|| provider.reset(finality.clone())).await;
        return Ok(());
    }

    // we are adding 1 to the height because we are fetching block by block, we also configured
    // the sequential cache to use increment == 1.
    let starting_height = last_recorded_height + 1;
    let ending_height = min(ending_height, MAX_PARENT_VIEW_BLOCK_GAP + starting_height);
    tracing::debug!("parent view range: {starting_height}-{ending_height}");

    let new_parent_views =
        get_new_parent_views(agent_proxy, starting_height, ending_height).await?;
    tracing::debug!("new parent views: {new_parent_views:?}");

    atomically_or_err::<_, Error, _>(move || {
        for (height, block_hash, validator_set, messages) in new_parent_views.clone() {
            provider.new_parent_view(height, block_hash, validator_set, messages)?;
        }
        Ok(())
    })
    .await?;

    tracing::debug!("updated new parent views till height: {ending_height}");

    Ok(())
}

/// Obtains the last recorded height from provider cache or from last committed finality height.
async fn last_recorded_height(
    provider: &Arc<Toggle<CachedFinalityProvider>>,
) -> anyhow::Result<Option<BlockHeight>> {
    let height = atomically(|| {
        let h = if let Some(h) = provider.latest_height()? {
            Some(h)
        } else if let Some(f) = provider.last_committed_finality()? {
            Some(f.height)
        } else {
            None
        };
        Ok(h)
    })
    .await;

    Ok(height)
}

/// Obtain the new parent views for the input block height range
async fn get_new_parent_views(
    agent_proxy: &Arc<IPCAgentProxy>,
    start_height: BlockHeight,
    end_height: BlockHeight,
) -> anyhow::Result<Vec<(BlockHeight, BlockHash, ValidatorSet, Vec<CrossMsg>)>> {
    let mut block_height_to_update = vec![];
    let mut total_top_down_msgs = 0;
    for h in start_height..=end_height {
        let block_hash = agent_proxy
            .get_block_hash(h)
            .await
            .context("cannot fetch block hash")?;
        let validator_set = agent_proxy
            .get_validator_set(h)
            .await
            .context("cannot fetch validator set")?;
        let top_down_msgs = agent_proxy
            .get_top_down_msgs(h, h)
            .await
            .context("cannot fetch top down messages")?;
        total_top_down_msgs += top_down_msgs.len();

        block_height_to_update.push((h, block_hash, validator_set, top_down_msgs));
        if total_top_down_msgs >= TOPDOWN_MSG_LEN_THRESHOLD {
            break;
        }
    }
    Ok(block_height_to_update)
}

/// The proxy to ipc agent
pub struct IPCAgentProxy {
    agent_client: IpcAgentClient<JsonRpcClientImpl>,
    /// The parent subnet for the child subnet we are target. We can derive from child subnet,
    /// but storing it separately so that we dont have to derive every time.
    parent_subnet: SubnetID,
    /// The child subnet that this node belongs to.
    child_subnet: SubnetID,
}

impl IPCAgentProxy {
    pub fn new(
        client: IpcAgentClient<JsonRpcClientImpl>,
        target_subnet: SubnetID,
    ) -> anyhow::Result<Self> {
        let parent = target_subnet
            .parent()
            .ok_or_else(|| anyhow!("subnet does not have parent"))?;
        Ok(Self {
            agent_client: client,
            parent_subnet: parent,
            child_subnet: target_subnet,
        })
    }

    pub async fn get_chain_head_height(&self) -> anyhow::Result<BlockHeight> {
        let height = self
            .agent_client
            .get_chain_head_height(&self.parent_subnet)
            .await?;
        Ok(height as BlockHeight)
    }

    /// Get the genesis epoch of the child subnet, i.e. the epoch that the subnet was created in
    /// the parent subnet.
    pub async fn get_genesis_epoch(&self) -> anyhow::Result<ChainEpoch> {
        let r = self
            .agent_client
            // passing None to height as we are fetching the latest data
            .get_validator_set(&self.child_subnet, None)
            .await?;
        Ok(r.genesis_epoch)
    }

    /// Getting the block hash at the target height.
    pub async fn get_block_hash(&self, height: BlockHeight) -> anyhow::Result<BlockHash> {
        self.agent_client
            .get_block_hash(&self.parent_subnet, height as ChainEpoch)
            .await
    }

    /// Get the top down messages from the starting to the ending height.
    pub async fn get_top_down_msgs(
        &self,
        start_height: BlockHeight,
        end_height: u64,
    ) -> anyhow::Result<Vec<CrossMsg>> {
        self.agent_client
            .get_top_down_msgs(
                &self.child_subnet,
                start_height as ChainEpoch,
                end_height as ChainEpoch,
            )
            .await
    }

    /// Get the validator set at the specified height.
    pub async fn get_validator_set(&self, height: BlockHeight) -> anyhow::Result<ValidatorSet> {
        let r = self
            .agent_client
            .get_validator_set(&self.child_subnet, Some(height as ChainEpoch))
            .await?;
        Ok(r.validator_set)
    }
}
