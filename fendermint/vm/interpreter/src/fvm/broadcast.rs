// Copyright 2022-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use anyhow::{anyhow, bail, Context};
use ethers::types as et;
use fvm_shared::error::ExitCode;
use fvm_shared::{address::Address, chainid::ChainID, econ::TokenAmount, BLOCK_GAS_LIMIT};
use num_traits::Zero;
use tendermint_rpc::Client;

use fendermint_crypto::SecretKey;
use fendermint_rpc::message::GasParams;
use fendermint_rpc::query::QueryClient;
use fendermint_rpc::tx::{CallClient, TxClient, TxCommit};
use fendermint_rpc::{client::FendermintClient, message::MessageFactory};
use fendermint_vm_message::query::FvmQueryHeight;

macro_rules! retry {
    ($max_retries:expr, $block:expr) => {{
        let mut attempt = 0;
        let value = loop {
            match $block {
                Err((code, msg)) if attempt == $max_retries || !can_retry(code) => {
                    bail!(msg);
                }
                Err((_, msg)) => {
                    tracing::warn!(error = msg, attempt, "retry broadcast");
                    attempt += 1;
                }
                Ok(value) => {
                    break value;
                }
            }
        };
        value
    }};
}

/// Broadcast transactions to Tendermint.
///
/// This is typically something only active validators would want to do
/// from within Fendermint as part of the block lifecycle, for example
/// to submit their signatures to the ledger.
///
/// The broadcaster encapsulates the tactics for figuring out the nonce,
/// the gas limit, potential retries, etc.
#[derive(Clone)]
pub struct Broadcaster<C> {
    client: FendermintClient<C>,
    secret_key: SecretKey,
    addr: Address,
    gas_fee_cap: TokenAmount,
    gas_premium: TokenAmount,
    max_retries: u8,
}

impl<C> Broadcaster<C>
where
    C: Client + Clone + Send + Sync,
{
    pub fn new(
        client: C,
        addr: Address,
        secret_key: SecretKey,
        gas_fee_cap: TokenAmount,
        gas_premium: TokenAmount,
    ) -> Self {
        let client = FendermintClient::new(client);
        Self {
            client,
            secret_key,
            addr,
            gas_fee_cap,
            gas_premium,
            max_retries: 0,
        }
    }

    pub fn with_max_retries(mut self, max_retries: u8) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub async fn fevm_invoke(
        &self,
        contract: Address,
        calldata: et::Bytes,
        chain_id: ChainID,
    ) -> anyhow::Result<()> {
        let tx_hash = retry!(self.max_retries, {
            let sequence = self
                .sequence()
                .await
                .context("failed to get broadcaster sequence")?;

            let factory =
                MessageFactory::new(self.secret_key.clone(), self.addr, sequence, chain_id);

            // Using the bound client as a one-shot transaction sender.
            let mut client = self.client.clone().bind(factory);

            // TODO: Maybe we should implement something like the Ethereum facade for estimating fees?
            // I don't want to call the Ethereum API directly (it would be one more dependency).
            // Another option is for Fendermint to recognise transactions coming from validators
            // and always put them into the block to facilitate checkpointing.
            let mut gas_params = GasParams {
                gas_limit: BLOCK_GAS_LIMIT,
                gas_fee_cap: self.gas_fee_cap.clone(),
                gas_premium: self.gas_premium.clone(),
            };

            // Not expecting to send any tokens to the contracts.
            let value = TokenAmount::zero();

            // We can use the `Committed` state to execute the message, which is more efficient than doing it on `Pending`.
            let gas_estimate = client
                .fevm_estimate_gas(
                    contract,
                    calldata.0.clone(),
                    value.clone(),
                    gas_params.clone(),
                    FvmQueryHeight::Committed,
                )
                .await
                .context("failed to estimate gas")?;

            if gas_estimate.value.exit_code.is_success() {
                gas_params.gas_limit = gas_estimate.value.gas_limit;
            } else {
                bail!(
                    "failed to estimate gas: {} - {}",
                    gas_estimate.value.exit_code,
                    gas_estimate.value.info
                );
            }

            let res = TxClient::<TxCommit>::fevm_invoke(
                &mut client,
                contract,
                calldata.0.clone(),
                value,
                gas_params,
            )
            .await
            .context("failed to invoke contract")?;

            if res.response.check_tx.code.is_err() {
                Err((
                    res.response.check_tx.code,
                    format!(
                        "broadcasted transaction failed during check: {} - {}",
                        res.response.check_tx.code.value(),
                        res.response.check_tx.info
                    ),
                ))
            } else if res.response.deliver_tx.code.is_err() {
                Err((
                    res.response.deliver_tx.code,
                    format!(
                        "broadcasted transaction failed during deliver: {} - {}",
                        res.response.deliver_tx.code.value(),
                        res.response.deliver_tx.info
                    ),
                ))
            } else {
                Ok(res.response.hash)
            }
        });

        tracing::debug!(?tx_hash, "fevm transaction committed");

        Ok(())
    }

    /// Fetch the current nonce to be used in the next message.
    async fn sequence(&self) -> anyhow::Result<u64> {
        // Using the `Pending` state to query just in case there are other transactions initiated by the validator.
        let res = self
            .client
            .actor_state(&self.addr, FvmQueryHeight::Pending)
            .await
            .context("failed to get broadcaster actor state")?;

        match res.value {
            Some((_, state)) => Ok(state.sequence),
            None => Err(anyhow!("broadcaster actor {} cannot be found", self.addr)),
        }
    }
}

/// Decide if it's worth retrying the transaction.
fn can_retry(code: tendermint::abci::Code) -> bool {
    match ExitCode::new(code.value()) {
        // If the sender doesn't exist it doesn't matter how many times we try.
        ExitCode::SYS_SENDER_INVALID => false,
        // If the nonce was invalid, it might be because of a race condition, and we can try again.
        ExitCode::SYS_SENDER_STATE_INVALID => true,
        // If the sender doesn't have enough funds to cover the gas, it's unlikely that repeating imemediately will help.
        ExitCode::SYS_INSUFFICIENT_FUNDS => false,
        ExitCode::USR_INSUFFICIENT_FUNDS => false,
        // If we estimate the gas wrong, there's no point trying it will probably go wrong again.
        ExitCode::SYS_OUT_OF_GAS => false,
        // Unknown errors should not be retried.
        _ => false,
    }
}