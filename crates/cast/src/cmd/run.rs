use alloy_consensus::Transaction;
use alloy_network::TransactionResponse;
use alloy_provider::Provider;
use alloy_rpc_types::BlockTransactions;
use clap::Parser;
use eyre::{OptionExt, Result, WrapErr};
use foundry_cli::{
    opts::{EtherscanOpts, RpcOpts},
    utils::{apply_block_changes, execute_block_transactions, handle_traces, TraceResult},
};
use foundry_common::{is_known_system_sender, shell, SYSTEM_TRANSACTION_TYPE};
use foundry_compilers::artifacts::EvmVersion;
use foundry_config::{
    figment::{
        self,
        value::{Dict, Map},
        Figment, Metadata, Profile,
    },
    Config,
};
use foundry_evm::{
    executors::TracingExecutor,
    opts::EvmOpts,
    traces::{InternalTraceMode, TraceMode},
    utils::configure_tx_env,
    Env,
};
use foundry_evm_core::env::AsEnvMut;

/// CLI arguments for `cast run`.
#[derive(Clone, Debug, Parser)]
pub struct RunArgs {
    /// The transaction hash.
    tx_hash: String,

    /// Opens the transaction in the debugger.
    #[arg(long, short)]
    debug: bool,

    /// Whether to identify internal functions in traces.
    #[arg(long)]
    decode_internal: bool,

    /// Print out opcode traces.
    #[arg(long, short)]
    trace_printer: bool,

    /// Executes the transaction only with the state from the previous block.
    ///
    /// May result in different results than the live execution!
    #[arg(long)]
    quick: bool,

    /// Label addresses in the trace.
    ///
    /// Example: 0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045:vitalik.eth
    #[arg(long, short)]
    label: Vec<String>,

    #[command(flatten)]
    etherscan: EtherscanOpts,

    #[command(flatten)]
    rpc: RpcOpts,

    /// The EVM version to use.
    ///
    /// Overrides the version specified in the config.
    #[arg(long)]
    evm_version: Option<EvmVersion>,

    /// Sets the number of assumed available compute units per second for this provider
    ///
    /// default value: 330
    ///
    /// See also, <https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second>
    #[arg(long, alias = "cups", value_name = "CUPS")]
    pub compute_units_per_second: Option<u64>,

    /// Disables rate limiting for this node's provider.
    ///
    /// default value: false
    ///
    /// See also, <https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second>
    #[arg(long, value_name = "NO_RATE_LIMITS", visible_alias = "no-rpc-rate-limit")]
    pub no_rate_limit: bool,

    /// Enables Odyssey features.
    #[arg(long, alias = "alphanet")]
    pub odyssey: bool,

    /// Use current project artifacts for trace decoding.
    #[arg(long, visible_alias = "la")]
    pub with_local_artifacts: bool,

    /// Disable block gas limit check.
    #[arg(long)]
    pub disable_block_gas_limit: bool,
}

impl RunArgs {
    /// Executes the transaction by replaying it
    ///
    /// This replays the entire block the transaction was mined in unless `quick` is set to true
    ///
    /// Note: This executes the transaction(s) as is: Cheatcodes are disabled
    pub async fn run(self) -> Result<()> {
        let figment = Into::<Figment>::into(&self.rpc).merge(&self);
        let evm_opts = figment.extract::<EvmOpts>()?;
        let mut config = Config::from_provider(figment)?.sanitized();

        let compute_units_per_second =
            if self.no_rate_limit { Some(u64::MAX) } else { self.compute_units_per_second };

        let provider = foundry_cli::utils::get_provider_builder(&config)?
            .compute_units_per_second_opt(compute_units_per_second)
            .build()?;

        let tx_hash = self.tx_hash.parse().wrap_err("invalid tx hash")?;
        let tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .wrap_err_with(|| format!("tx not found: {tx_hash:?}"))?
            .ok_or_else(|| eyre::eyre!("tx not found: {:?}", tx_hash))?;

        // check if the tx is a system transaction
        if is_known_system_sender(tx.from()) ||
            tx.transaction_type() == Some(SYSTEM_TRANSACTION_TYPE)
        {
            return Err(eyre::eyre!(
                "{:?} is a system transaction.\nReplaying system transactions is currently not supported.",
                tx.tx_hash()
            ));
        }

        let tx_block_number =
            tx.block_number.ok_or_else(|| eyre::eyre!("tx may still be pending: {:?}", tx_hash))?;

        // fetch the block the transaction was mined in
        let block = provider.get_block(tx_block_number.into()).full().await?;

        // we need to fork off the parent block
        config.fork_block_number = Some(tx_block_number - 1);

        let create2_deployer = evm_opts.create2_deployer;
        let (mut env, fork, chain, odyssey) =
            TracingExecutor::get_fork_material(&config, evm_opts).await?;
        let mut evm_version = self.evm_version;

        env.evm_env.cfg_env.disable_block_gas_limit = self.disable_block_gas_limit;
        env.evm_env.block_env.number = tx_block_number;

        if let Some(block) = &block {
            apply_block_changes(&mut env, &mut evm_version, block);
        }

        let trace_mode = TraceMode::Call
            .with_debug(self.debug)
            .with_decode_internal(if self.decode_internal {
                InternalTraceMode::Full
            } else {
                InternalTraceMode::None
            })
            .with_state_changes(shell::verbosity() > 4);
        let mut executor = TracingExecutor::new(
            env.clone(),
            fork,
            evm_version,
            trace_mode,
            odyssey,
            create2_deployer,
            None,
        )?;
        let mut env = Env::new_with_spec_id(
            env.evm_env.cfg_env.clone(),
            env.evm_env.block_env.clone(),
            env.tx.clone(),
            executor.spec_id(),
        );

        // Set the state to the moment right before the transaction
        if !self.quick {
            if !shell::is_json() {
                sh_println!("Executing previous transactions from the block.")?;
            }

            if let Some(block) = block {
                let BlockTransactions::Full(ref txs) = block.transactions else {
                    return Err(eyre::eyre!("Could not get block txs"))
                };

                // Find the tx index of the transaction.
                let tx_index = txs
                    .iter()
                    .position(|tx| tx.tx_hash() == tx_hash)
                    .ok_or_eyre("Could not find transaction in block")?;

                execute_block_transactions(&mut env, &mut executor, &txs[..tx_index])?;
            }
        }

        // Execute our transaction
        let result = {
            executor.set_trace_printer(self.trace_printer);

            configure_tx_env(&mut env.as_env_mut(), &tx.inner);

            if let Some(to) = Transaction::to(&tx) {
                trace!(tx=?tx.tx_hash(), to=?to, "executing call transaction");
                TraceResult::try_from(executor.transact_with_env(env))?
            } else {
                trace!(tx=?tx.tx_hash(), "executing create transaction");
                TraceResult::try_from(executor.deploy_with_env(env, None))?
            }
        };

        handle_traces(
            result,
            &config,
            chain,
            self.label,
            self.with_local_artifacts,
            self.debug,
            self.decode_internal,
        )
        .await?;

        Ok(())
    }
}

impl figment::Provider for RunArgs {
    fn metadata(&self) -> Metadata {
        Metadata::named("RunArgs")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut map = Map::new();

        if self.odyssey {
            map.insert("odyssey".into(), self.odyssey.into());
        }

        if let Some(api_key) = &self.etherscan.key {
            map.insert("etherscan_api_key".into(), api_key.as_str().into());
        }

        if let Some(api_version) = &self.etherscan.api_version {
            map.insert("etherscan_api_version".into(), api_version.to_string().into());
        }

        if let Some(evm_version) = self.evm_version {
            map.insert("evm_version".into(), figment::value::Value::serialize(evm_version)?);
        }

        Ok(Map::from([(Config::selected_profile(), map)]))
    }
}
