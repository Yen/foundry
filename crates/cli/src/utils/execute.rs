use alloy_consensus::Transaction;
use alloy_provider::network::{AnyNetwork, AnyRpcBlock, AnyRpcTransaction, TransactionResponse};
use eyre::Context;
use foundry_common::{is_known_system_sender, SYSTEM_TRANSACTION_TYPE};
use foundry_compilers::artifacts::EvmVersion;
use foundry_evm::{
    executors::{EvmError, TracingExecutor},
    utils::{apply_chain_and_block_specific_env_changes, configure_tx_env},
    Env,
};
use foundry_evm_core::env::AsEnvMut;

use crate::utils::init_progress;

pub fn execute_block_transactions(
    env: &mut Env,
    executor: &mut TracingExecutor,
    txs: &[AnyRpcTransaction],
) -> eyre::Result<()> {
    let pb = init_progress(txs.len() as u64, "tx");
    pb.set_position(0);

    for (index, tx) in txs.iter().enumerate() {
        // System transactions such as on L2s don't contain any pricing info so
        // we skip them otherwise this would cause
        // reverts
        if is_known_system_sender(tx.from()) ||
            tx.transaction_type() == Some(SYSTEM_TRANSACTION_TYPE)
        {
            pb.set_position((index + 1) as u64);
            continue;
        }

        configure_tx_env(&mut env.as_env_mut(), &tx.inner);

        if let Some(to) = Transaction::to(tx) {
            trace!(tx=?tx.tx_hash(),?to, "executing previous call transaction");
            executor.transact_with_env(env.clone()).wrap_err_with(|| {
                format!(
                    "Failed to execute transaction: {:?} in block {}",
                    tx.tx_hash(),
                    env.evm_env.block_env.number
                )
            })?;
        } else {
            trace!(tx=?tx.tx_hash(), "executing previous create transaction");
            if let Err(error) = executor.deploy_with_env(env.clone(), None) {
                match error {
                    // Reverted transactions should be skipped
                    EvmError::Execution(_) => (),
                    error => {
                        return Err(error).wrap_err_with(|| {
                            format!(
                                "Failed to deploy transaction: {:?} in block {}",
                                tx.tx_hash(),
                                env.evm_env.block_env.number
                            )
                        })
                    }
                }
            }
        }

        pb.set_position((index + 1) as u64);
    }

    Ok(())
}

pub fn apply_block_changes(
    env: &mut Env,
    evm_version: &mut Option<EvmVersion>,
    block: &AnyRpcBlock,
) {
    env.evm_env.block_env.number = block.header.number;
    env.evm_env.block_env.timestamp = block.header.timestamp;
    env.evm_env.block_env.beneficiary = block.header.beneficiary;
    env.evm_env.block_env.difficulty = block.header.difficulty;
    env.evm_env.block_env.prevrandao = Some(block.header.mix_hash.unwrap_or_default());
    env.evm_env.block_env.basefee = block.header.base_fee_per_gas.unwrap_or_default();
    env.evm_env.block_env.gas_limit = block.header.gas_limit;

    // TODO: we need a smarter way to map the block to the corresponding evm_version for
    // commonly used chains
    if evm_version.is_none() {
        // if the block has the excess_blob_gas field, we assume it's a Cancun block
        if block.header.excess_blob_gas.is_some() {
            *evm_version = Some(EvmVersion::Prague);
        }
    }
    apply_chain_and_block_specific_env_changes::<AnyNetwork>(env.as_env_mut(), block);
}
