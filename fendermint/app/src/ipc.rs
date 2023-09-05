// Copyright 2022-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT
//! IPC related execution

use crate::app::{AppState, AppStoreKey};
use crate::{App, BlockHeight};
use bytes::Bytes;
use fendermint_rpc::response::decode_fevm_invoke_bytes;
use fendermint_storage::{Codec, Encode, KVReadable, KVStore, KVWritable};
use fendermint_vm_actor_interface::{ipc, system};
use fendermint_vm_interpreter::fvm::state::FvmStateParams;
use fendermint_vm_interpreter::fvm::FvmMessage;
use fendermint_vm_topdown::convert::{
    decode_parent_finality_return, encode_get_latest_parent_finality,
};
use fendermint_vm_topdown::sync::ParentFinalityStateQuery;
use fendermint_vm_topdown::{IPCParentFinality, ParentFinalityProvider};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::RawBytes;
use fvm_shared::econ::TokenAmount;
use num_traits::Zero;

/// Queries the LATEST COMMITTED parent finality from the storage
pub struct AppParentFinalityQuery<DB, SS, S, I, P>
where
    SS: Blockstore + 'static,
    S: KVStore,
{
    /// The app to get state
    app: App<DB, SS, S, I, P>,
}

impl<DB, SS, S, I, P> AppParentFinalityQuery<DB, SS, S, I, P>
where
    S: KVStore
        + Codec<AppState>
        + Encode<AppStoreKey>
        + Encode<BlockHeight>
        + Codec<FvmStateParams>,
    DB: KVWritable<S> + KVReadable<S> + 'static + Clone,
    SS: Blockstore + 'static + Clone,
    P: ParentFinalityProvider + Send + Sync,
{
    pub fn new(app: App<DB, SS, S, I, P>) -> Self {
        Self { app }
    }
}

impl<DB, SS, S, I, P> ParentFinalityStateQuery for AppParentFinalityQuery<DB, SS, S, I, P>
where
    S: KVStore
        + Codec<AppState>
        + Encode<AppStoreKey>
        + Encode<BlockHeight>
        + Codec<FvmStateParams>,
    DB: KVWritable<S> + KVReadable<S> + 'static + Clone,
    SS: Blockstore + 'static + Clone,
    P: ParentFinalityProvider + Send + Sync,
{
    fn get_latest_committed_finality(&self) -> anyhow::Result<Option<IPCParentFinality>> {
        let maybe_exec_state = self.app.new_read_only_exec_state()?;

        let finality = if let Some(mut exec_state) = maybe_exec_state {
            let msg = implicit_gateway_message(encode_get_latest_parent_finality()?);
            let (apply_ret, _) = exec_state.execute_implicit(msg)?;

            let data = apply_ret.msg_receipt.return_data.to_vec();
            let decoded = decode_fevm_invoke_bytes(&Bytes::from(data))?;
            Some(decode_parent_finality_return(decoded.as_slice())?)
        } else {
            None
        };

        Ok(finality)
    }
}

#[inline]
fn implicit_gateway_message(params: Vec<u8>) -> FvmMessage {
    FvmMessage {
        version: 0,
        from: system::SYSTEM_ACTOR_ADDR,
        to: ipc::GATEWAY_ACTOR_ADDR,
        value: TokenAmount::zero(),
        method_num: ipc::gateway::METHOD_INVOKE_CONTRACT,
        params: RawBytes::new(params),
        // we are sending a implicit message, no need to set sequence
        sequence: 0,
        gas_limit: fvm_shared::BLOCK_GAS_LIMIT,
        gas_fee_cap: TokenAmount::zero(),
        gas_premium: TokenAmount::zero(),
    }
}
