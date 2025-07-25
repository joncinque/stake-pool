#![allow(clippy::arithmetic_side_effects)]
mod client;
mod output;

use {
    crate::{
        client::*,
        output::{CliStakePool, CliStakePoolDetails, CliStakePoolStakeAccountInfo, CliStakePools},
    },
    bincode::deserialize,
    clap::{
        crate_description, crate_name, crate_version, value_t, value_t_or_exit, App, AppSettings,
        Arg, ArgGroup, ArgMatches, SubCommand,
    },
    solana_clap_utils::{
        compute_unit_price::{compute_unit_price_arg, COMPUTE_UNIT_PRICE_ARG},
        input_parsers::{keypair_of, pubkey_of},
        input_validators::{
            is_amount, is_keypair_or_ask_keyword, is_parsable, is_pubkey, is_url,
            is_valid_percentage, is_valid_pubkey, is_valid_signer,
        },
        keypair::{signer_from_path_with_config, SignerFromPathConfig},
        ArgConstant,
    },
    solana_cli_output::OutputFormat,
    solana_client::rpc_client::RpcClient,
    solana_program::{
        borsh1::{get_instance_packed_len, get_packed_len},
        instruction::Instruction,
        program_pack::Pack,
        pubkey::Pubkey,
    },
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        compute_budget::ComputeBudgetInstruction,
        hash::Hash,
        message::Message,
        native_token::{self, Sol},
        signature::{Keypair, Signer},
        signers::Signers,
        transaction::Transaction,
    },
    solana_stake_interface as stake,
    solana_system_interface::instruction as system_instruction,
    spl_associated_token_account::instruction::create_associated_token_account,
    spl_associated_token_account_client::address::get_associated_token_address_with_program_id,
    spl_stake_pool::{
        self, find_stake_program_address, find_transient_stake_program_address,
        find_withdraw_authority_program_address,
        instruction::{FundingType, PreferredValidatorType},
        minimum_delegation,
        state::{Fee, FeeType, StakePool, ValidatorList, ValidatorStakeInfo},
        MINIMUM_RESERVE_LAMPORTS,
    },
    spl_token_2022::{
        check_spl_token_program_account, extension::StateWithExtensions, state::Mint,
    },
    std::{cmp::Ordering, num::NonZeroU32, process::exit, rc::Rc},
};

pub(crate) struct Config {
    stake_pool_program_id: Pubkey,
    rpc_client: RpcClient,
    verbose: bool,
    output_format: OutputFormat,
    manager: Box<dyn Signer>,
    staker: Box<dyn Signer>,
    funding_authority: Option<Box<dyn Signer>>,
    token_owner: Box<dyn Signer>,
    fee_payer: Box<dyn Signer>,
    dry_run: bool,
    no_update: bool,
    compute_unit_price: Option<u64>,
    compute_unit_limit: ComputeUnitLimit,
}

type CommandResult = Result<(), Error>;

const STAKE_STATE_LEN: usize = 200;

macro_rules! unique_signers {
    ($vec:ident) => {
        $vec.sort_by_key(|l| l.pubkey());
        $vec.dedup();
    };
}

fn default_stake_pool_id(json_rpc_url: &str) -> Pubkey {
    if json_rpc_url.contains("devnet") {
        spl_stake_pool::devnet::id()
    } else {
        spl_stake_pool::id()
    }
}

fn check_fee_payer_balance(config: &Config, required_balance: u64) -> Result<(), Error> {
    let balance = config.rpc_client.get_balance(&config.fee_payer.pubkey())?;
    if balance < required_balance {
        Err(format!(
            "Fee payer, {}, has insufficient balance: {} required, {} available",
            config.fee_payer.pubkey(),
            Sol(required_balance),
            Sol(balance)
        )
        .into())
    } else {
        Ok(())
    }
}

const FEES_REFERENCE: &str = "Consider setting a minimal fee. \
                              See https://spl.solana.com/stake-pool/fees for more \
                              information about fees and best practices. If you are \
                              aware of the possible risks of a stake pool with no fees, \
                              you may force pool creation with the --unsafe-fees flag.";

enum ComputeUnitLimit {
    Default,
    Static(u32),
    Simulated,
}
const COMPUTE_UNIT_LIMIT_ARG: ArgConstant<'static> = ArgConstant {
    name: "compute_unit_limit",
    long: "--with-compute-unit-limit",
    help: "Set compute unit limit for transaction, in compute units; also accepts \
        keyword DEFAULT to use the default compute unit limit, which is 200k per \
        top-level instruction, with a maximum of 1.4 million. \
        If nothing is set, transactions are simulated prior to sending, and the \
        compute units consumed are set as the limit. This may may fail if accounts \
        are modified by another transaction between simulation and execution.",
};
fn is_compute_unit_limit_or_simulated<T>(string: T) -> Result<(), String>
where
    T: AsRef<str> + std::fmt::Display,
{
    if string.as_ref().parse::<u32>().is_ok() || string.as_ref() == "DEFAULT" {
        Ok(())
    } else {
        Err(format!(
            "Unable to parse input compute unit limit as integer or DEFAULT, provided: {string}"
        ))
    }
}
fn parse_compute_unit_limit<T>(string: T) -> Result<ComputeUnitLimit, String>
where
    T: AsRef<str> + std::fmt::Display,
{
    match string.as_ref().parse::<u32>() {
        Ok(compute_unit_limit) => Ok(ComputeUnitLimit::Static(compute_unit_limit)),
        Err(_) if string.as_ref() == "DEFAULT" => Ok(ComputeUnitLimit::Default),
        _ => Err(format!(
            "Unable to parse compute unit limit, provided: {string}"
        )),
    }
}

fn check_stake_pool_fees(
    epoch_fee: &Fee,
    withdrawal_fee: &Fee,
    deposit_fee: &Fee,
) -> Result<(), Error> {
    if epoch_fee.numerator == 0 || epoch_fee.denominator == 0 {
        return Err(format!("Epoch fee should not be 0. {}", FEES_REFERENCE,).into());
    }
    let is_withdrawal_fee_zero = withdrawal_fee.numerator == 0 || withdrawal_fee.denominator == 0;
    let is_deposit_fee_zero = deposit_fee.numerator == 0 || deposit_fee.denominator == 0;
    if is_withdrawal_fee_zero && is_deposit_fee_zero {
        return Err(format!(
            "Withdrawal and deposit fee should not both be 0. {}",
            FEES_REFERENCE,
        )
        .into());
    }
    Ok(())
}

fn get_signer(
    matches: &ArgMatches<'_>,
    keypair_name: &str,
    keypair_path: &str,
    wallet_manager: &mut Option<Rc<RemoteWalletManager>>,
    signer_from_path_config: SignerFromPathConfig,
) -> Box<dyn Signer> {
    signer_from_path_with_config(
        matches,
        matches.value_of(keypair_name).unwrap_or(keypair_path),
        keypair_name,
        wallet_manager,
        &signer_from_path_config,
    )
    .unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        exit(1);
    })
}

fn get_latest_blockhash(client: &RpcClient) -> Result<Hash, Error> {
    Ok(client
        .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?
        .0)
}

fn send_transaction_no_wait(
    config: &Config,
    transaction: Transaction,
) -> solana_client::client_error::Result<()> {
    if config.dry_run {
        let result = config.rpc_client.simulate_transaction(&transaction)?;
        println!("Simulate result: {:?}", result);
    } else {
        let signature = config.rpc_client.send_transaction(&transaction)?;
        println!("Signature: {}", signature);
    }
    Ok(())
}

fn send_transaction(
    config: &Config,
    transaction: Transaction,
) -> solana_client::client_error::Result<()> {
    if config.dry_run {
        let result = config.rpc_client.simulate_transaction(&transaction)?;
        println!("Simulate result: {:?}", result);
    } else {
        let signature = config
            .rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)?;
        println!("Signature: {}", signature);
    }
    Ok(())
}

fn checked_transaction_with_signers_and_additional_fee<T: Signers>(
    config: &Config,
    instructions: &[Instruction],
    signers: &T,
    additional_fee: u64,
) -> Result<Transaction, Error> {
    let recent_blockhash = get_latest_blockhash(&config.rpc_client)?;
    let mut instructions = instructions.to_vec();
    if let Some(compute_unit_price) = config.compute_unit_price {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
            compute_unit_price,
        ));
    }
    match config.compute_unit_limit {
        ComputeUnitLimit::Default => {}
        ComputeUnitLimit::Static(compute_unit_limit) => {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                compute_unit_limit,
            ));
        }
        ComputeUnitLimit::Simulated => {
            add_compute_unit_limit_from_simulation(
                &config.rpc_client,
                &mut instructions,
                &config.fee_payer.pubkey(),
                &recent_blockhash,
            )?;
        }
    }
    let message = Message::new_with_blockhash(
        &instructions,
        Some(&config.fee_payer.pubkey()),
        &recent_blockhash,
    );
    check_fee_payer_balance(
        config,
        additional_fee.saturating_add(config.rpc_client.get_fee_for_message(&message)?),
    )?;
    let transaction = Transaction::new(signers, message, recent_blockhash);
    Ok(transaction)
}

fn checked_transaction_with_signers<T: Signers>(
    config: &Config,
    instructions: &[Instruction],
    signers: &T,
) -> Result<Transaction, Error> {
    checked_transaction_with_signers_and_additional_fee(config, instructions, signers, 0)
}

fn new_stake_account(
    fee_payer: &Pubkey,
    instructions: &mut Vec<Instruction>,
    lamports: u64,
) -> Keypair {
    // Account for tokens not specified, creating one
    let stake_receiver_keypair = Keypair::new();
    let stake_receiver_pubkey = stake_receiver_keypair.pubkey();
    println!(
        "Creating account to receive stake {}",
        stake_receiver_pubkey
    );

    instructions.push(
        // Creating new account
        system_instruction::create_account(
            fee_payer,
            &stake_receiver_pubkey,
            lamports,
            STAKE_STATE_LEN as u64,
            &stake::program::id(),
        ),
    );

    stake_receiver_keypair
}

fn setup_reserve_stake_account(
    config: &Config,
    reserve_keypair: &Keypair,
    reserve_stake_balance: u64,
    withdraw_authority: &Pubkey,
) -> CommandResult {
    let reserve_account_info = config.rpc_client.get_account(&reserve_keypair.pubkey());
    if let Ok(account) = reserve_account_info {
        if account.owner == stake::program::id() {
            if account.data.iter().any(|&x| x != 0) {
                println!(
                    "Reserve stake account {} already exists and is initialized",
                    reserve_keypair.pubkey()
                );
                return Ok(());
            } else {
                let instructions = vec![stake::instruction::initialize(
                    &reserve_keypair.pubkey(),
                    &stake::state::Authorized {
                        staker: *withdraw_authority,
                        withdrawer: *withdraw_authority,
                    },
                    &stake::state::Lockup::default(),
                )];
                let signers = vec![config.fee_payer.as_ref()];
                let transaction =
                    checked_transaction_with_signers(config, &instructions, &signers)?;
                println!(
                    "Initializing existing reserve stake account {}",
                    reserve_keypair.pubkey()
                );
                send_transaction(config, transaction)?;
                return Ok(());
            }
        }
    }

    let instructions = vec![
        system_instruction::create_account(
            &config.fee_payer.pubkey(),
            &reserve_keypair.pubkey(),
            reserve_stake_balance,
            STAKE_STATE_LEN as u64,
            &stake::program::id(),
        ),
        stake::instruction::initialize(
            &reserve_keypair.pubkey(),
            &stake::state::Authorized {
                staker: *withdraw_authority,
                withdrawer: *withdraw_authority,
            },
            &stake::state::Lockup::default(),
        ),
    ];

    let signers = vec![config.fee_payer.as_ref(), reserve_keypair];
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;

    println!(
        "Creating and initializing reserve stake account {}",
        reserve_keypair.pubkey()
    );
    send_transaction(config, transaction)?;
    Ok(())
}

/// Creates the stake pool mint if it doesn't exist, returns the token program
/// id used
fn setup_mint_account(
    config: &Config,
    mint_keypair: &Keypair,
    mint_account_balance: u64,
    withdraw_authority: &Pubkey,
    default_decimals: u8,
) -> Result<Pubkey, Error> {
    let mint_account_info = config.rpc_client.get_account(&mint_keypair.pubkey());
    if let Ok(account) = mint_account_info {
        if check_spl_token_program_account(&account.owner).is_ok() {
            if account.data.iter().any(|&x| x != 0) {
                if let Ok(mint) = StateWithExtensions::<Mint>::unpack(&account.data) {
                    if Option::from(mint.base.mint_authority) != Some(*withdraw_authority) {
                        return Err(format!(
                            "Mint account exists with the incorrect mint authority. Set mint authority with `spl-token authorize {} mint {}",
                            mint_keypair.pubkey(), withdraw_authority
                        ).into());
                    }
                } else {
                    return Err(format!(
                        "Account {} already exists, but is not a valid mint",
                        mint_keypair.pubkey()
                    )
                    .into());
                }
            } else {
                let instructions = vec![spl_token_2022::instruction::initialize_mint(
                    &account.owner,
                    &mint_keypair.pubkey(),
                    withdraw_authority,
                    None,
                    default_decimals,
                )?];
                let signers = vec![config.fee_payer.as_ref()];
                let transaction =
                    checked_transaction_with_signers(config, &instructions, &signers)?;
                println!(
                    "Initializing existing mint account {}",
                    mint_keypair.pubkey()
                );
                send_transaction(config, transaction)?;
            }
            return Ok(account.owner);
        }
    }

    let instructions = vec![
        system_instruction::create_account(
            &config.fee_payer.pubkey(),
            &mint_keypair.pubkey(),
            mint_account_balance,
            spl_token_2022::state::Mint::LEN as u64,
            &spl_token::id(),
        ),
        spl_token_2022::instruction::initialize_mint(
            &spl_token::id(),
            &mint_keypair.pubkey(),
            withdraw_authority,
            None,
            default_decimals,
        )?,
    ];

    let signers = vec![config.fee_payer.as_ref(), mint_keypair];
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;

    println!(
        "Creating and initializing mint account {}",
        mint_keypair.pubkey()
    );
    send_transaction(config, transaction)?;
    Ok(spl_token::id())
}

fn setup_pool_fee_account(
    config: &Config,
    mint_pubkey: &Pubkey,
    token_program_id: &Pubkey,
    total_rent_free_balances: &mut u64,
) -> CommandResult {
    let pool_fee_account = get_associated_token_address_with_program_id(
        &config.manager.pubkey(),
        mint_pubkey,
        token_program_id,
    );
    let pool_fee_account_info = config.rpc_client.get_account(&pool_fee_account);
    if let Ok(account) = pool_fee_account_info {
        if check_spl_token_program_account(&account.owner).is_ok() {
            println!("Pool fee account {} already exists", pool_fee_account);
            return Ok(());
        }
    }
    // Create pool fee account
    let mut instructions = vec![];
    add_associated_token_account(
        config,
        mint_pubkey,
        token_program_id,
        &config.manager.pubkey(),
        &mut instructions,
        total_rent_free_balances,
    );

    println!("Creating pool fee collection account {}", pool_fee_account);

    let signers = vec![config.fee_payer.as_ref()];
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;

    send_transaction(config, transaction)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn setup_and_initialize_validator_list_with_stake_pool(
    config: &Config,
    stake_pool_keypair: &Keypair,
    validator_list_keypair: &Keypair,
    reserve_keypair: &Keypair,
    mint_keypair: &Keypair,
    token_program_id: &Pubkey,
    pool_fee_account: &Pubkey,
    deposit_authority: Option<Keypair>,
    epoch_fee: Fee,
    withdrawal_fee: Fee,
    deposit_fee: Fee,
    referral_fee: u8,
    max_validators: u32,
    withdraw_authority: &Pubkey,
    validator_list_balance: u64,
    validator_list_size: usize,
) -> CommandResult {
    let stake_pool_account_info = config.rpc_client.get_account(&stake_pool_keypair.pubkey());
    let validator_list_account_info = config
        .rpc_client
        .get_account(&validator_list_keypair.pubkey());

    let stake_pool_account_lamports = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(get_packed_len::<StakePool>())?;

    let mut instructions = vec![];
    let mut signers = vec![config.fee_payer.as_ref(), config.manager.as_ref()];

    if let Ok(account) = validator_list_account_info {
        if account.owner == config.stake_pool_program_id {
            if account.data.iter().all(|&x| x == 0) {
                println!(
                    "Validator list account {} already exists and is ready to be initialized",
                    validator_list_keypair.pubkey()
                );
            } else {
                println!(
                    "Validator list account {} already exists and is initialized",
                    validator_list_keypair.pubkey()
                );
                return Ok(());
            }
        }
    } else {
        instructions.push(system_instruction::create_account(
            &config.fee_payer.pubkey(),
            &validator_list_keypair.pubkey(),
            validator_list_balance,
            validator_list_size as u64,
            &config.stake_pool_program_id,
        ));
        signers.push(validator_list_keypair);
    }

    if let Ok(account) = stake_pool_account_info {
        if account.owner == config.stake_pool_program_id {
            if account.data.iter().all(|&x| x == 0) {
                println!(
                    "Stake pool account {} already exists but is not initialized",
                    stake_pool_keypair.pubkey()
                );
            } else {
                println!(
                    "Stake pool account {} already exists and is initialized",
                    stake_pool_keypair.pubkey()
                );
                return Ok(());
            }
        }
    } else {
        instructions.push(system_instruction::create_account(
            &config.fee_payer.pubkey(),
            &stake_pool_keypair.pubkey(),
            stake_pool_account_lamports,
            get_packed_len::<StakePool>() as u64,
            &config.stake_pool_program_id,
        ));
    }
    instructions.push(spl_stake_pool::instruction::initialize(
        &config.stake_pool_program_id,
        &stake_pool_keypair.pubkey(),
        &config.manager.pubkey(),
        &config.staker.pubkey(),
        withdraw_authority,
        &validator_list_keypair.pubkey(),
        &reserve_keypair.pubkey(),
        &mint_keypair.pubkey(),
        pool_fee_account,
        token_program_id,
        deposit_authority.as_ref().map(|x| x.pubkey()),
        epoch_fee,
        withdrawal_fee,
        deposit_fee,
        referral_fee,
        max_validators,
    ));
    signers.push(stake_pool_keypair);

    if let Some(ref deposit_auth) = deposit_authority {
        signers.push(deposit_auth);
        println!(
            "Deposits will be restricted to {} only, this can be changed using the set-funding-authority command.",
            deposit_auth.pubkey()
        );
    }

    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;

    println!(
        "Setting up and initializing stake pool account {} with validator list {}",
        stake_pool_keypair.pubkey(),
        validator_list_keypair.pubkey()
    );
    send_transaction(config, transaction)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn command_create_pool(
    config: &Config,
    deposit_authority: Option<Keypair>,
    epoch_fee: Fee,
    withdrawal_fee: Fee,
    deposit_fee: Fee,
    referral_fee: u8,
    max_validators: u32,
    stake_pool_keypair: Option<Keypair>,
    validator_list_keypair: Option<Keypair>,
    mint_keypair: Option<Keypair>,
    reserve_keypair: Option<Keypair>,
    unsafe_fees: bool,
) -> CommandResult {
    if !unsafe_fees {
        check_stake_pool_fees(&epoch_fee, &withdrawal_fee, &deposit_fee)?;
    }

    let reserve_keypair = reserve_keypair.unwrap_or_else(Keypair::new);
    let mint_keypair = mint_keypair.unwrap_or_else(Keypair::new);
    let stake_pool_keypair = stake_pool_keypair.unwrap_or_else(Keypair::new);
    let validator_list_keypair = validator_list_keypair.unwrap_or_else(Keypair::new);

    let reserve_stake_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(STAKE_STATE_LEN)?
        + MINIMUM_RESERVE_LAMPORTS;
    let mint_account_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(spl_token_2022::state::Mint::LEN)?;
    let pool_fee_account_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(spl_token_2022::state::Account::LEN)?;
    let stake_pool_account_lamports = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(get_packed_len::<StakePool>())?;
    let empty_validator_list = ValidatorList::new(max_validators);
    let validator_list_size = get_instance_packed_len(&empty_validator_list)?;
    let validator_list_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(validator_list_size)?;
    let mut total_rent_free_balances = reserve_stake_balance
        + mint_account_balance
        + pool_fee_account_balance
        + stake_pool_account_lamports
        + validator_list_balance;

    let default_decimals = spl_token_2022::native_mint::DECIMALS;

    // Calculate withdraw authority used for minting pool tokens
    let (withdraw_authority, _) = find_withdraw_authority_program_address(
        &config.stake_pool_program_id,
        &stake_pool_keypair.pubkey(),
    );

    if config.verbose {
        println!("Stake pool withdraw authority {}", withdraw_authority);
    }

    setup_reserve_stake_account(
        config,
        &reserve_keypair,
        reserve_stake_balance,
        &withdraw_authority,
    )?;
    let token_program_id = setup_mint_account(
        config,
        &mint_keypair,
        mint_account_balance,
        &withdraw_authority,
        default_decimals,
    )?;
    setup_pool_fee_account(
        config,
        &mint_keypair.pubkey(),
        &token_program_id,
        &mut total_rent_free_balances,
    )?;

    let pool_fee_account = get_associated_token_address_with_program_id(
        &config.manager.pubkey(),
        &mint_keypair.pubkey(),
        &token_program_id,
    );

    setup_and_initialize_validator_list_with_stake_pool(
        config,
        &stake_pool_keypair,
        &validator_list_keypair,
        &reserve_keypair,
        &mint_keypair,
        &token_program_id,
        &pool_fee_account,
        deposit_authority,
        epoch_fee,
        withdrawal_fee,
        deposit_fee,
        referral_fee,
        max_validators,
        &withdraw_authority,
        validator_list_balance,
        validator_list_size,
    )?;

    Ok(())
}

fn create_token_metadata(
    config: &Config,
    stake_pool_address: &Pubkey,
    name: String,
    symbol: String,
    uri: String,
) -> CommandResult {
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;

    let mut signers = vec![config.fee_payer.as_ref(), config.manager.as_ref()];
    let instructions = vec![spl_stake_pool::instruction::create_token_metadata(
        &config.stake_pool_program_id,
        stake_pool_address,
        &stake_pool.manager,
        &stake_pool.pool_mint,
        &config.fee_payer.pubkey(),
        name,
        symbol,
        uri,
    )];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn update_token_metadata(
    config: &Config,
    stake_pool_address: &Pubkey,
    name: String,
    symbol: String,
    uri: String,
) -> CommandResult {
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;

    let mut signers = vec![config.fee_payer.as_ref(), config.manager.as_ref()];
    let instructions = vec![spl_stake_pool::instruction::update_token_metadata(
        &config.stake_pool_program_id,
        stake_pool_address,
        &stake_pool.manager,
        &stake_pool.pool_mint,
        name,
        symbol,
        uri,
    )];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_vsa_add(
    config: &Config,
    stake_pool_address: &Pubkey,
    vote_account: &Pubkey,
) -> CommandResult {
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    if validator_list.contains(vote_account) {
        println!(
            "Stake pool already contains validator {}, ignoring",
            vote_account
        );
        return Ok(());
    }

    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    // iterate until a free account is found
    let (stake_account_address, validator_seed) = {
        let mut i = 0;
        loop {
            let seed = NonZeroU32::new(i);
            let (address, _) = find_stake_program_address(
                &config.stake_pool_program_id,
                vote_account,
                stake_pool_address,
                seed,
            );
            let maybe_account = config
                .rpc_client
                .get_account_with_commitment(
                    &stake_pool.reserve_stake,
                    config.rpc_client.commitment(),
                )?
                .value;
            if maybe_account.is_some() {
                break (address, seed);
            }
            i += 1;
        }
    };
    println!(
        "Adding stake account {}, delegated to {}",
        stake_account_address, vote_account
    );

    let mut signers = vec![config.fee_payer.as_ref(), config.staker.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[
            spl_stake_pool::instruction::add_validator_to_pool_with_vote(
                &config.stake_pool_program_id,
                &stake_pool,
                stake_pool_address,
                vote_account,
                validator_seed,
            ),
        ],
        &signers,
    )?;

    send_transaction(config, transaction)?;
    Ok(())
}

fn command_vsa_remove(
    config: &Config,
    stake_pool_address: &Pubkey,
    vote_account: &Pubkey,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    let validator_stake_info = validator_list
        .find(vote_account)
        .ok_or("Vote account not found in validator list")?;

    let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());
    let (stake_account_address, _) = find_stake_program_address(
        &config.stake_pool_program_id,
        vote_account,
        stake_pool_address,
        validator_seed,
    );
    println!(
        "Removing stake account {}, delegated to {}",
        stake_account_address, vote_account
    );

    let mut signers = vec![config.fee_payer.as_ref(), config.staker.as_ref()];
    let instructions = vec![
        // Create new validator stake account address
        spl_stake_pool::instruction::remove_validator_from_pool_with_vote(
            &config.stake_pool_program_id,
            &stake_pool,
            stake_pool_address,
            vote_account,
            validator_seed,
            validator_stake_info.transient_seed_suffix.into(),
        ),
    ];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_increase_validator_stake(
    config: &Config,
    stake_pool_address: &Pubkey,
    vote_account: &Pubkey,
    lamports: u64,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    let validator_stake_info = validator_list
        .find(vote_account)
        .ok_or("Vote account not found in validator list")?;
    let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());

    let mut signers = vec![config.fee_payer.as_ref(), config.staker.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[
            spl_stake_pool::instruction::increase_validator_stake_with_vote(
                &config.stake_pool_program_id,
                &stake_pool,
                stake_pool_address,
                vote_account,
                lamports,
                validator_seed,
                validator_stake_info.transient_seed_suffix.into(),
            ),
        ],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_decrease_validator_stake(
    config: &Config,
    stake_pool_address: &Pubkey,
    vote_account: &Pubkey,
    lamports: u64,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    let validator_stake_info = validator_list
        .find(vote_account)
        .ok_or("Vote account not found in validator list")?;
    let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());

    let mut signers = vec![config.fee_payer.as_ref(), config.staker.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[
            spl_stake_pool::instruction::decrease_validator_stake_with_vote(
                &config.stake_pool_program_id,
                &stake_pool,
                stake_pool_address,
                vote_account,
                lamports,
                validator_seed,
                validator_stake_info.transient_seed_suffix.into(),
            ),
        ],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_set_preferred_validator(
    config: &Config,
    stake_pool_address: &Pubkey,
    preferred_type: PreferredValidatorType,
    vote_address: Option<Pubkey>,
) -> CommandResult {
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let mut signers = vec![config.fee_payer.as_ref(), config.staker.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[spl_stake_pool::instruction::set_preferred_validator(
            &config.stake_pool_program_id,
            stake_pool_address,
            &config.staker.pubkey(),
            &stake_pool.validator_list,
            preferred_type,
            vote_address,
        )],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn add_associated_token_account(
    config: &Config,
    mint: &Pubkey,
    token_program_id: &Pubkey,
    owner: &Pubkey,
    instructions: &mut Vec<Instruction>,
    rent_free_balances: &mut u64,
) -> Pubkey {
    // Account for tokens not specified, creating one
    let account = get_associated_token_address_with_program_id(owner, mint, token_program_id);
    if get_token_account(&config.rpc_client, &account, mint).is_err() {
        println!("Creating associated token account {} to receive stake pool tokens of mint {}, owned by {}", account, mint, owner);

        let min_account_balance = config
            .rpc_client
            .get_minimum_balance_for_rent_exemption(spl_token_2022::state::Account::LEN)
            .unwrap();

        instructions.push(create_associated_token_account(
            &config.fee_payer.pubkey(),
            owner,
            mint,
            token_program_id,
        ));

        *rent_free_balances += min_account_balance;
    } else {
        println!("Using existing associated token account {} to receive stake pool tokens of mint {}, owned by {}", account, mint, owner);
    }

    account
}

fn command_deposit_stake(
    config: &Config,
    stake_pool_address: &Pubkey,
    stake: &Pubkey,
    withdraw_authority: Box<dyn Signer>,
    pool_token_receiver_account: &Option<Pubkey>,
    referrer_token_account: &Option<Pubkey>,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let stake_state = get_stake_state(&config.rpc_client, stake)?;

    if config.verbose {
        println!("Depositing stake account {:?}", stake_state);
    }
    let vote_account = match stake_state {
        stake::state::StakeStateV2::Stake(_, stake, _) => Ok(stake.delegation.voter_pubkey),
        _ => Err("Wrong stake account state, must be delegated to validator"),
    }?;

    // Check if this vote account has staking account in the pool
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    let validator_stake_info = validator_list
        .find(&vote_account)
        .ok_or("Vote account not found in the stake pool")?;
    let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());

    // Calculate validator stake account address linked to the pool
    let (validator_stake_account, _) = find_stake_program_address(
        &config.stake_pool_program_id,
        &vote_account,
        stake_pool_address,
        validator_seed,
    );

    let validator_stake_state = get_stake_state(&config.rpc_client, &validator_stake_account)?;
    println!(
        "Depositing stake {} into stake pool account {}",
        stake, validator_stake_account
    );
    if config.verbose {
        println!("{:?}", validator_stake_state);
    }

    let mut instructions: Vec<Instruction> = vec![];
    let mut signers = vec![config.fee_payer.as_ref(), withdraw_authority.as_ref()];

    let mut total_rent_free_balances: u64 = 0;

    // Create token account if not specified
    let pool_token_receiver_account =
        pool_token_receiver_account.unwrap_or(add_associated_token_account(
            config,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            &config.token_owner.pubkey(),
            &mut instructions,
            &mut total_rent_free_balances,
        ));

    let referrer_token_account = referrer_token_account.unwrap_or(pool_token_receiver_account);

    let pool_withdraw_authority =
        find_withdraw_authority_program_address(&config.stake_pool_program_id, stake_pool_address)
            .0;

    let mut deposit_instructions =
        if let Some(stake_deposit_authority) = config.funding_authority.as_ref() {
            signers.push(stake_deposit_authority.as_ref());
            if stake_deposit_authority.pubkey() != stake_pool.stake_deposit_authority {
                let error = format!(
                    "Invalid deposit authority specified, expected {}, received {}",
                    stake_pool.stake_deposit_authority,
                    stake_deposit_authority.pubkey()
                );
                return Err(error.into());
            }

            spl_stake_pool::instruction::deposit_stake_with_authority(
                &config.stake_pool_program_id,
                stake_pool_address,
                &stake_pool.validator_list,
                &stake_deposit_authority.pubkey(),
                &pool_withdraw_authority,
                stake,
                &withdraw_authority.pubkey(),
                &validator_stake_account,
                &stake_pool.reserve_stake,
                &pool_token_receiver_account,
                &stake_pool.manager_fee_account,
                &referrer_token_account,
                &stake_pool.pool_mint,
                &stake_pool.token_program_id,
            )
        } else {
            spl_stake_pool::instruction::deposit_stake(
                &config.stake_pool_program_id,
                stake_pool_address,
                &stake_pool.validator_list,
                &pool_withdraw_authority,
                stake,
                &withdraw_authority.pubkey(),
                &validator_stake_account,
                &stake_pool.reserve_stake,
                &pool_token_receiver_account,
                &stake_pool.manager_fee_account,
                &referrer_token_account,
                &stake_pool.pool_mint,
                &stake_pool.token_program_id,
            )
        };

    instructions.append(&mut deposit_instructions);

    unique_signers!(signers);
    let transaction = checked_transaction_with_signers_and_additional_fee(
        config,
        &instructions,
        &signers,
        total_rent_free_balances,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_deposit_all_stake(
    config: &Config,
    stake_pool_address: &Pubkey,
    stake_authority: &Pubkey,
    withdraw_authority: Box<dyn Signer>,
    pool_token_receiver_account: &Option<Pubkey>,
    referrer_token_account: &Option<Pubkey>,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_addresses = get_all_stake(&config.rpc_client, stake_authority)?;
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;

    // Create token account if not specified
    let mut total_rent_free_balances = 0;
    let mut create_token_account_instructions = vec![];
    let pool_token_receiver_account =
        pool_token_receiver_account.unwrap_or(add_associated_token_account(
            config,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            &config.token_owner.pubkey(),
            &mut create_token_account_instructions,
            &mut total_rent_free_balances,
        ));
    if !create_token_account_instructions.is_empty() {
        let transaction = checked_transaction_with_signers_and_additional_fee(
            config,
            &create_token_account_instructions,
            &[config.fee_payer.as_ref()],
            total_rent_free_balances,
        )?;
        send_transaction(config, transaction)?;
    }

    let referrer_token_account = referrer_token_account.unwrap_or(pool_token_receiver_account);

    let pool_withdraw_authority =
        find_withdraw_authority_program_address(&config.stake_pool_program_id, stake_pool_address)
            .0;
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    let mut signers = if let Some(stake_deposit_authority) = config.funding_authority.as_ref() {
        if stake_deposit_authority.pubkey() != stake_pool.stake_deposit_authority {
            let error = format!(
                "Invalid deposit authority specified, expected {}, received {}",
                stake_pool.stake_deposit_authority,
                stake_deposit_authority.pubkey()
            );
            return Err(error.into());
        }

        vec![
            config.fee_payer.as_ref(),
            withdraw_authority.as_ref(),
            stake_deposit_authority.as_ref(),
        ]
    } else {
        vec![config.fee_payer.as_ref(), withdraw_authority.as_ref()]
    };
    unique_signers!(signers);

    for stake_address in stake_addresses {
        let stake_state = get_stake_state(&config.rpc_client, &stake_address)?;

        let vote_account = match stake_state {
            stake::state::StakeStateV2::Stake(_, stake, _) => Ok(stake.delegation.voter_pubkey),
            _ => Err("Wrong stake account state, must be delegated to validator"),
        }?;

        let validator_stake_info = validator_list
            .find(&vote_account)
            .ok_or("Vote account not found in the stake pool")?;
        let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());

        // Calculate validator stake account address linked to the pool
        let (validator_stake_account, _) = find_stake_program_address(
            &config.stake_pool_program_id,
            &vote_account,
            stake_pool_address,
            validator_seed,
        );

        let validator_stake_state = get_stake_state(&config.rpc_client, &validator_stake_account)?;
        println!("Depositing user stake {}: {:?}", stake_address, stake_state);
        println!(
            "..into pool stake {}: {:?}",
            validator_stake_account, validator_stake_state
        );

        let instructions = if let Some(stake_deposit_authority) = config.funding_authority.as_ref()
        {
            spl_stake_pool::instruction::deposit_stake_with_authority(
                &config.stake_pool_program_id,
                stake_pool_address,
                &stake_pool.validator_list,
                &stake_deposit_authority.pubkey(),
                &pool_withdraw_authority,
                &stake_address,
                &withdraw_authority.pubkey(),
                &validator_stake_account,
                &stake_pool.reserve_stake,
                &pool_token_receiver_account,
                &stake_pool.manager_fee_account,
                &referrer_token_account,
                &stake_pool.pool_mint,
                &stake_pool.token_program_id,
            )
        } else {
            spl_stake_pool::instruction::deposit_stake(
                &config.stake_pool_program_id,
                stake_pool_address,
                &stake_pool.validator_list,
                &pool_withdraw_authority,
                &stake_address,
                &withdraw_authority.pubkey(),
                &validator_stake_account,
                &stake_pool.reserve_stake,
                &pool_token_receiver_account,
                &stake_pool.manager_fee_account,
                &referrer_token_account,
                &stake_pool.pool_mint,
                &stake_pool.token_program_id,
            )
        };

        let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;
        send_transaction(config, transaction)?;
    }
    Ok(())
}

fn command_deposit_sol(
    config: &Config,
    stake_pool_address: &Pubkey,
    from: &Option<Keypair>,
    pool_token_receiver_account: &Option<Pubkey>,
    referrer_token_account: &Option<Pubkey>,
    lamports: u64,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    // Check withdraw_from balance
    let from_pubkey = from
        .as_ref()
        .map_or_else(|| config.fee_payer.pubkey(), |keypair| keypair.pubkey());
    let from_balance = config.rpc_client.get_balance(&from_pubkey)?;
    if from_balance < lamports {
        return Err(format!(
            "Not enough SOL to deposit into pool: {}.\nMaximum deposit amount is {} SOL.",
            Sol(lamports),
            Sol(from_balance)
        )
        .into());
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;

    let mut instructions: Vec<Instruction> = vec![];

    // ephemeral SOL account just to do the transfer
    let user_sol_transfer = Keypair::new();
    let mut signers = vec![config.fee_payer.as_ref(), &user_sol_transfer];
    if let Some(keypair) = from.as_ref() {
        signers.push(keypair)
    }

    let mut total_rent_free_balances: u64 = 0;

    // Create the ephemeral SOL account
    instructions.push(system_instruction::transfer(
        &from_pubkey,
        &user_sol_transfer.pubkey(),
        lamports,
    ));

    // Create token account if not specified
    let pool_token_receiver_account =
        pool_token_receiver_account.unwrap_or(add_associated_token_account(
            config,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            &config.token_owner.pubkey(),
            &mut instructions,
            &mut total_rent_free_balances,
        ));

    let referrer_token_account = referrer_token_account.unwrap_or(pool_token_receiver_account);

    let pool_withdraw_authority =
        find_withdraw_authority_program_address(&config.stake_pool_program_id, stake_pool_address)
            .0;

    let deposit_instruction = if let Some(deposit_authority) = config.funding_authority.as_ref() {
        let expected_sol_deposit_authority = stake_pool.sol_deposit_authority.ok_or_else(|| {
            "SOL deposit authority specified in arguments but stake pool has none".to_string()
        })?;
        signers.push(deposit_authority.as_ref());
        if deposit_authority.pubkey() != expected_sol_deposit_authority {
            let error = format!(
                "Invalid deposit authority specified, expected {}, received {}",
                expected_sol_deposit_authority,
                deposit_authority.pubkey()
            );
            return Err(error.into());
        }

        spl_stake_pool::instruction::deposit_sol_with_authority(
            &config.stake_pool_program_id,
            stake_pool_address,
            &deposit_authority.pubkey(),
            &pool_withdraw_authority,
            &stake_pool.reserve_stake,
            &user_sol_transfer.pubkey(),
            &pool_token_receiver_account,
            &stake_pool.manager_fee_account,
            &referrer_token_account,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            lamports,
        )
    } else {
        spl_stake_pool::instruction::deposit_sol(
            &config.stake_pool_program_id,
            stake_pool_address,
            &pool_withdraw_authority,
            &stake_pool.reserve_stake,
            &user_sol_transfer.pubkey(),
            &pool_token_receiver_account,
            &stake_pool.manager_fee_account,
            &referrer_token_account,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            lamports,
        )
    };

    instructions.push(deposit_instruction);

    unique_signers!(signers);
    let transaction = checked_transaction_with_signers_and_additional_fee(
        config,
        &instructions,
        &signers,
        total_rent_free_balances,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_list(config: &Config, stake_pool_address: &Pubkey) -> CommandResult {
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let reserve_stake_account_address = stake_pool.reserve_stake.to_string();
    let total_lamports = stake_pool.total_lamports;
    let last_update_epoch = stake_pool.last_update_epoch;
    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
    let max_number_of_validators = validator_list.header.max_validators;
    let current_number_of_validators = validator_list.validators.len();
    let pool_mint = get_token_mint(&config.rpc_client, &stake_pool.pool_mint)?;
    let epoch_info = config.rpc_client.get_epoch_info()?;
    let pool_withdraw_authority =
        find_withdraw_authority_program_address(&config.stake_pool_program_id, stake_pool_address)
            .0;
    let reserve_stake = config.rpc_client.get_account(&stake_pool.reserve_stake)?;
    let minimum_reserve_stake_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(STAKE_STATE_LEN)?
        + MINIMUM_RESERVE_LAMPORTS;
    let cli_stake_pool_stake_account_infos = validator_list
        .validators
        .iter()
        .map(|validator| {
            let validator_seed = NonZeroU32::new(validator.validator_seed_suffix.into());
            let (stake_account_address, _) = find_stake_program_address(
                &config.stake_pool_program_id,
                &validator.vote_account_address,
                stake_pool_address,
                validator_seed,
            );
            let (transient_stake_account_address, _) = find_transient_stake_program_address(
                &config.stake_pool_program_id,
                &validator.vote_account_address,
                stake_pool_address,
                validator.transient_seed_suffix.into(),
            );
            let update_required = u64::from(validator.last_update_epoch) != epoch_info.epoch;
            CliStakePoolStakeAccountInfo {
                vote_account_address: validator.vote_account_address.to_string(),
                stake_account_address: stake_account_address.to_string(),
                validator_active_stake_lamports: validator.active_stake_lamports.into(),
                validator_last_update_epoch: validator.last_update_epoch.into(),
                validator_lamports: validator.stake_lamports().unwrap(),
                validator_transient_stake_account_address: transient_stake_account_address
                    .to_string(),
                validator_transient_stake_lamports: validator.transient_stake_lamports.into(),
                update_required,
            }
        })
        .collect();
    let total_pool_tokens =
        spl_token_2022::amount_to_ui_amount(stake_pool.pool_token_supply, pool_mint.decimals);
    let mut cli_stake_pool = CliStakePool::from((
        *stake_pool_address,
        stake_pool,
        validator_list,
        pool_withdraw_authority,
    ));
    let update_required = last_update_epoch != epoch_info.epoch;
    let cli_stake_pool_details = CliStakePoolDetails {
        reserve_stake_account_address,
        reserve_stake_lamports: reserve_stake.lamports,
        minimum_reserve_stake_balance,
        stake_accounts: cli_stake_pool_stake_account_infos,
        total_lamports,
        total_pool_tokens,
        current_number_of_validators: current_number_of_validators as u32,
        max_number_of_validators,
        update_required,
    };
    cli_stake_pool.details = Some(cli_stake_pool_details);
    println!("{}", config.output_format.formatted_string(&cli_stake_pool));
    Ok(())
}

fn command_update(
    config: &Config,
    stake_pool_address: &Pubkey,
    force: bool,
    no_merge: bool,
    stale_only: bool,
) -> CommandResult {
    if config.no_update {
        println!("Update requested, but --no-update flag specified, so doing nothing");
        return Ok(());
    }
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let epoch_info = config.rpc_client.get_epoch_info()?;

    if stake_pool.last_update_epoch == epoch_info.epoch {
        if force {
            println!("Update not required, but --force flag specified, so doing it anyway");
        } else {
            println!("Update not required");
            return Ok(());
        }
    }

    let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;

    let (mut update_list_instructions, final_instructions) = if stale_only {
        spl_stake_pool::instruction::update_stale_stake_pool(
            &config.stake_pool_program_id,
            &stake_pool,
            &validator_list,
            stake_pool_address,
            no_merge,
            epoch_info.epoch,
        )
    } else {
        spl_stake_pool::instruction::update_stake_pool(
            &config.stake_pool_program_id,
            &stake_pool,
            &validator_list,
            stake_pool_address,
            no_merge,
        )
    };

    let update_list_instructions_len = update_list_instructions.len();
    if update_list_instructions_len > 0 {
        let last_instruction = update_list_instructions.split_off(update_list_instructions_len - 1);
        // send the first ones without waiting
        for instruction in update_list_instructions {
            let transaction = checked_transaction_with_signers(
                config,
                &[instruction],
                &[config.fee_payer.as_ref()],
            )?;
            send_transaction_no_wait(config, transaction)?;
        }

        // wait on the last one
        let transaction = checked_transaction_with_signers(
            config,
            &last_instruction,
            &[config.fee_payer.as_ref()],
        )?;
        send_transaction(config, transaction)?;
    }
    let transaction = checked_transaction_with_signers(
        config,
        &final_instructions,
        &[config.fee_payer.as_ref()],
    )?;
    send_transaction(config, transaction)?;

    Ok(())
}

#[derive(PartialEq, Debug)]
struct WithdrawAccount {
    stake_address: Pubkey,
    vote_address: Option<Pubkey>,
    pool_amount: u64,
}

fn sorted_accounts<F>(
    validator_list: &ValidatorList,
    stake_pool: &StakePool,
    get_info: F,
) -> Vec<(Pubkey, u64, Option<Pubkey>)>
where
    F: Fn(&ValidatorStakeInfo) -> (Pubkey, u64, Option<Pubkey>),
{
    let mut result: Vec<(Pubkey, u64, Option<Pubkey>)> = validator_list
        .validators
        .iter()
        .map(get_info)
        .collect::<Vec<_>>();

    result.sort_by(|left, right| {
        if left.2 == stake_pool.preferred_withdraw_validator_vote_address {
            Ordering::Less
        } else if right.2 == stake_pool.preferred_withdraw_validator_vote_address {
            Ordering::Greater
        } else {
            right.1.cmp(&left.1)
        }
    });

    result
}

fn prepare_withdraw_accounts(
    config: &Config,
    stake_pool: &StakePool,
    pool_amount: u64,
    stake_pool_address: &Pubkey,
    skip_fee: bool,
) -> Result<Vec<WithdrawAccount>, Error> {
    let stake_minimum_delegation = config.rpc_client.get_stake_minimum_delegation()?;
    let stake_pool_minimum_delegation = minimum_delegation(stake_minimum_delegation);
    let min_balance = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(STAKE_STATE_LEN)?
        .saturating_add(stake_pool_minimum_delegation);
    let pool_mint = get_token_mint(&config.rpc_client, &stake_pool.pool_mint)?;
    let validator_list: ValidatorList =
        get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;

    let mut accounts: Vec<(Pubkey, u64, Option<Pubkey>)> = Vec::new();

    accounts.append(&mut sorted_accounts(
        &validator_list,
        stake_pool,
        |validator| {
            let validator_seed = NonZeroU32::new(validator.validator_seed_suffix.into());
            let (stake_account_address, _) = find_stake_program_address(
                &config.stake_pool_program_id,
                &validator.vote_account_address,
                stake_pool_address,
                validator_seed,
            );

            (
                stake_account_address,
                validator.active_stake_lamports.into(),
                Some(validator.vote_account_address),
            )
        },
    ));

    accounts.append(&mut sorted_accounts(
        &validator_list,
        stake_pool,
        |validator| {
            let (transient_stake_account_address, _) = find_transient_stake_program_address(
                &config.stake_pool_program_id,
                &validator.vote_account_address,
                stake_pool_address,
                validator.transient_seed_suffix.into(),
            );

            (
                transient_stake_account_address,
                u64::from(validator.transient_stake_lamports).saturating_sub(min_balance),
                Some(validator.vote_account_address),
            )
        },
    ));

    let reserve_stake = config.rpc_client.get_account(&stake_pool.reserve_stake)?;

    accounts.push((
        stake_pool.reserve_stake,
        reserve_stake.lamports
            - config
                .rpc_client
                .get_minimum_balance_for_rent_exemption(STAKE_STATE_LEN)?
            - MINIMUM_RESERVE_LAMPORTS,
        None,
    ));

    // Prepare the list of accounts to withdraw from
    let mut withdraw_from: Vec<WithdrawAccount> = vec![];
    let mut remaining_amount = pool_amount;

    let fee = stake_pool.stake_withdrawal_fee;
    let inverse_fee = Fee {
        numerator: fee.denominator - fee.numerator,
        denominator: fee.denominator,
    };

    // Go through available accounts and withdraw from largest to smallest
    for (stake_address, lamports, vote_address_opt) in accounts {
        if lamports <= min_balance {
            continue;
        }

        let available_for_withdrawal_wo_fee =
            stake_pool.calc_pool_tokens_for_deposit(lamports).unwrap();

        let available_for_withdrawal = if skip_fee {
            available_for_withdrawal_wo_fee
        } else {
            available_for_withdrawal_wo_fee * inverse_fee.denominator / inverse_fee.numerator
        };

        let pool_amount = u64::min(available_for_withdrawal, remaining_amount);

        // Those accounts will be withdrawn completely with `claim` instruction
        withdraw_from.push(WithdrawAccount {
            stake_address,
            vote_address: vote_address_opt,
            pool_amount,
        });
        remaining_amount -= pool_amount;

        if remaining_amount == 0 {
            break;
        }
    }

    // Not enough stake to withdraw the specified amount
    if remaining_amount > 0 {
        return Err(format!(
            "No stake accounts found in this pool with enough balance to withdraw {} pool tokens.",
            spl_token_2022::amount_to_ui_amount(pool_amount, pool_mint.decimals)
        )
        .into());
    }

    Ok(withdraw_from)
}

fn command_withdraw_stake(
    config: &Config,
    stake_pool_address: &Pubkey,
    use_reserve: bool,
    vote_account_address: &Option<Pubkey>,
    stake_receiver_param: &Option<Pubkey>,
    pool_token_account: &Option<Pubkey>,
    pool_amount: f64,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let pool_mint = get_token_mint(&config.rpc_client, &stake_pool.pool_mint)?;
    let pool_amount = spl_token_2022::ui_amount_to_amount(pool_amount, pool_mint.decimals);

    let pool_withdraw_authority =
        find_withdraw_authority_program_address(&config.stake_pool_program_id, stake_pool_address)
            .0;

    let pool_token_account =
        pool_token_account.unwrap_or(get_associated_token_address_with_program_id(
            &config.token_owner.pubkey(),
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
        ));
    let token_account = get_token_account(
        &config.rpc_client,
        &pool_token_account,
        &stake_pool.pool_mint,
    )?;
    let stake_account_rent_exemption = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(STAKE_STATE_LEN)?;

    // Check withdraw_from balance
    if token_account.amount < pool_amount {
        return Err(format!(
            "Not enough token balance to withdraw {} pool tokens.\nMaximum withdraw amount is {} pool tokens.",
            spl_token_2022::amount_to_ui_amount(pool_amount, pool_mint.decimals),
            spl_token_2022::amount_to_ui_amount(token_account.amount, pool_mint.decimals)
        )
        .into());
    }

    // Check for the delegated stake receiver
    let maybe_stake_receiver_state = stake_receiver_param
        .map(|stake_receiver_pubkey| {
            let stake_account = config.rpc_client.get_account(&stake_receiver_pubkey).ok()?;
            let stake_state: stake::state::StakeStateV2 =
                deserialize(stake_account.data.as_slice())
                    .map_err(|err| {
                        format!("Invalid stake account {}: {}", stake_receiver_pubkey, err)
                    })
                    .ok()?;
            if stake_state.delegation().is_some() && stake_account.owner == stake::program::id() {
                Some(stake_state)
            } else {
                None
            }
        })
        .flatten();

    let stake_minimum_delegation = config.rpc_client.get_stake_minimum_delegation()?;
    let stake_pool_minimum_delegation = minimum_delegation(stake_minimum_delegation);

    let withdraw_accounts = if use_reserve {
        vec![WithdrawAccount {
            stake_address: stake_pool.reserve_stake,
            vote_address: None,
            pool_amount,
        }]
    } else if maybe_stake_receiver_state.is_some() {
        let vote_account = maybe_stake_receiver_state
            .unwrap()
            .delegation()
            .unwrap()
            .voter_pubkey;
        if let Some(vote_account_address) = vote_account_address {
            if *vote_account_address != vote_account {
                return Err(format!("Provided withdrawal vote account {} does not match delegation on stake receiver account {},
                remove this flag or provide a different stake account delegated to {}", vote_account_address, vote_account, vote_account_address).into());
            }
        }
        // Check if the vote account exists in the stake pool
        let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
        let validator_stake_info = validator_list
            .find(&vote_account)
            .ok_or(format!("Provided stake account is delegated to a vote account {} which does not exist in the stake pool", vote_account))?;
        let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());
        let (stake_account_address, _) = find_stake_program_address(
            &config.stake_pool_program_id,
            &vote_account,
            stake_pool_address,
            validator_seed,
        );
        let stake_account = config.rpc_client.get_account(&stake_account_address)?;

        let available_for_withdrawal = stake_pool
            .calc_lamports_withdraw_amount(
                stake_account
                    .lamports
                    .saturating_sub(stake_pool_minimum_delegation)
                    .saturating_sub(stake_account_rent_exemption),
            )
            .unwrap();

        if available_for_withdrawal < pool_amount {
            return Err(format!(
                "Not enough lamports available for withdrawal from {}, {} asked, {} available",
                stake_account_address, pool_amount, available_for_withdrawal
            )
            .into());
        }
        vec![WithdrawAccount {
            stake_address: stake_account_address,
            vote_address: Some(vote_account),
            pool_amount,
        }]
    } else if let Some(vote_account_address) = vote_account_address {
        let validator_list = get_validator_list(&config.rpc_client, &stake_pool.validator_list)?;
        let validator_stake_info = validator_list.find(vote_account_address).ok_or(format!(
            "Provided vote account address {} does not exist in the stake pool",
            vote_account_address
        ))?;
        let validator_seed = NonZeroU32::new(validator_stake_info.validator_seed_suffix.into());
        let (stake_account_address, _) = find_stake_program_address(
            &config.stake_pool_program_id,
            vote_account_address,
            stake_pool_address,
            validator_seed,
        );
        let stake_account = config.rpc_client.get_account(&stake_account_address)?;

        let available_for_withdrawal = stake_pool
            .calc_lamports_withdraw_amount(
                stake_account
                    .lamports
                    .saturating_sub(stake_pool_minimum_delegation)
                    .saturating_sub(stake_account_rent_exemption),
            )
            .unwrap();

        if available_for_withdrawal < pool_amount {
            return Err(format!(
                "Not enough lamports available for withdrawal from {}, {} asked, {} available",
                stake_account_address, pool_amount, available_for_withdrawal
            )
            .into());
        }
        vec![WithdrawAccount {
            stake_address: stake_account_address,
            vote_address: Some(*vote_account_address),
            pool_amount,
        }]
    } else {
        // Get the list of accounts to withdraw from
        prepare_withdraw_accounts(
            config,
            &stake_pool,
            pool_amount,
            stake_pool_address,
            stake_pool.manager_fee_account == pool_token_account,
        )?
    };

    // Construct transaction to withdraw from withdraw_accounts account list
    let mut instructions: Vec<Instruction> = vec![];
    let user_transfer_authority = Keypair::new(); // ephemeral keypair just to do the transfer
    let mut signers = vec![
        config.fee_payer.as_ref(),
        config.token_owner.as_ref(),
        &user_transfer_authority,
    ];
    let mut new_stake_keypairs = vec![];

    instructions.push(
        // Approve spending token
        spl_token_2022::instruction::approve(
            &stake_pool.token_program_id,
            &pool_token_account,
            &user_transfer_authority.pubkey(),
            &config.token_owner.pubkey(),
            &[],
            pool_amount,
        )?,
    );

    let mut total_rent_free_balances = 0;
    // Go through prepared accounts and withdraw/claim them
    for withdraw_account in withdraw_accounts {
        // Convert pool tokens amount to lamports
        let sol_withdraw_amount = stake_pool
            .calc_lamports_withdraw_amount(withdraw_account.pool_amount)
            .unwrap();

        if let Some(vote_address) = withdraw_account.vote_address {
            println!(
                "Withdrawing {}, or {} pool tokens, from stake account {}, delegated to {}",
                Sol(sol_withdraw_amount),
                spl_token_2022::amount_to_ui_amount(
                    withdraw_account.pool_amount,
                    pool_mint.decimals
                ),
                withdraw_account.stake_address,
                vote_address,
            );
        } else {
            println!(
                "Withdrawing {}, or {} pool tokens, from stake account {}",
                Sol(sol_withdraw_amount),
                spl_token_2022::amount_to_ui_amount(
                    withdraw_account.pool_amount,
                    pool_mint.decimals
                ),
                withdraw_account.stake_address,
            );
        }
        let stake_receiver =
            if (stake_receiver_param.is_none()) || (maybe_stake_receiver_state.is_some()) {
                // Creating new account to split the stake into new account
                let stake_keypair = new_stake_account(
                    &config.fee_payer.pubkey(),
                    &mut instructions,
                    stake_account_rent_exemption,
                );
                let stake_pubkey = stake_keypair.pubkey();
                total_rent_free_balances += stake_account_rent_exemption;
                new_stake_keypairs.push(stake_keypair);
                stake_pubkey
            } else {
                stake_receiver_param.unwrap()
            };

        instructions.push(spl_stake_pool::instruction::withdraw_stake(
            &config.stake_pool_program_id,
            stake_pool_address,
            &stake_pool.validator_list,
            &pool_withdraw_authority,
            &withdraw_account.stake_address,
            &stake_receiver,
            &config.staker.pubkey(),
            &user_transfer_authority.pubkey(),
            &pool_token_account,
            &stake_pool.manager_fee_account,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            withdraw_account.pool_amount,
        ));
    }

    // Merging the stake with account provided by user
    if maybe_stake_receiver_state.is_some() {
        for new_stake_keypair in &new_stake_keypairs {
            instructions.extend(stake::instruction::merge(
                &stake_receiver_param.unwrap(),
                &new_stake_keypair.pubkey(),
                &config.fee_payer.pubkey(),
            ));
        }
    }

    for new_stake_keypair in &new_stake_keypairs {
        signers.push(new_stake_keypair);
    }
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers_and_additional_fee(
        config,
        &instructions,
        &signers,
        total_rent_free_balances,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_withdraw_sol(
    config: &Config,
    stake_pool_address: &Pubkey,
    pool_token_account: &Option<Pubkey>,
    sol_receiver: &Pubkey,
    pool_amount: f64,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }

    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;
    let pool_mint = get_token_mint(&config.rpc_client, &stake_pool.pool_mint)?;
    let pool_amount = spl_token_2022::ui_amount_to_amount(pool_amount, pool_mint.decimals);

    let pool_token_account =
        pool_token_account.unwrap_or(get_associated_token_address_with_program_id(
            &config.token_owner.pubkey(),
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
        ));
    let token_account = get_token_account(
        &config.rpc_client,
        &pool_token_account,
        &stake_pool.pool_mint,
    )?;

    // Check withdraw_from balance
    if token_account.amount < pool_amount {
        return Err(format!(
            "Not enough token balance to withdraw {} pool tokens.\nMaximum withdraw amount is {} pool tokens.",
            spl_token_2022::amount_to_ui_amount(pool_amount, pool_mint.decimals),
            spl_token_2022::amount_to_ui_amount(token_account.amount, pool_mint.decimals)
        )
        .into());
    }

    // Construct transaction to withdraw from withdraw_accounts account list
    let user_transfer_authority = Keypair::new(); // ephemeral keypair just to do the transfer
    let mut signers = vec![
        config.fee_payer.as_ref(),
        config.token_owner.as_ref(),
        &user_transfer_authority,
    ];

    let mut instructions = vec![
        // Approve spending token
        spl_token_2022::instruction::approve(
            &stake_pool.token_program_id,
            &pool_token_account,
            &user_transfer_authority.pubkey(),
            &config.token_owner.pubkey(),
            &[],
            pool_amount,
        )?,
    ];

    let pool_withdraw_authority =
        find_withdraw_authority_program_address(&config.stake_pool_program_id, stake_pool_address)
            .0;

    let withdraw_instruction = if let Some(withdraw_authority) = config.funding_authority.as_ref() {
        let expected_sol_withdraw_authority =
            stake_pool.sol_withdraw_authority.ok_or_else(|| {
                "SOL withdraw authority specified in arguments but stake pool has none".to_string()
            })?;
        signers.push(withdraw_authority.as_ref());
        if withdraw_authority.pubkey() != expected_sol_withdraw_authority {
            let error = format!(
                "Invalid deposit withdraw specified, expected {}, received {}",
                expected_sol_withdraw_authority,
                withdraw_authority.pubkey()
            );
            return Err(error.into());
        }

        spl_stake_pool::instruction::withdraw_sol_with_authority(
            &config.stake_pool_program_id,
            stake_pool_address,
            &withdraw_authority.pubkey(),
            &pool_withdraw_authority,
            &user_transfer_authority.pubkey(),
            &pool_token_account,
            &stake_pool.reserve_stake,
            sol_receiver,
            &stake_pool.manager_fee_account,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            pool_amount,
        )
    } else {
        spl_stake_pool::instruction::withdraw_sol(
            &config.stake_pool_program_id,
            stake_pool_address,
            &pool_withdraw_authority,
            &user_transfer_authority.pubkey(),
            &pool_token_account,
            &stake_pool.reserve_stake,
            sol_receiver,
            &stake_pool.manager_fee_account,
            &stake_pool.pool_mint,
            &stake_pool.token_program_id,
            pool_amount,
        )
    };

    instructions.push(withdraw_instruction);

    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(config, &instructions, &signers)?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_set_manager(
    config: &Config,
    stake_pool_address: &Pubkey,
    new_manager: &Option<Box<dyn Signer>>,
    new_fee_receiver: &Option<Pubkey>,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }
    let stake_pool = get_stake_pool(&config.rpc_client, stake_pool_address)?;

    // If new accounts are missing in the arguments use the old ones
    let (new_manager_pubkey, mut signers): (Pubkey, Vec<&dyn Signer>) = match new_manager {
        None => (stake_pool.manager, vec![]),
        Some(value) => (value.pubkey(), vec![value.as_ref()]),
    };

    let new_fee_receiver = match new_fee_receiver {
        None => stake_pool.manager_fee_account,
        Some(value) => {
            // Check for fee receiver being a valid token account and have to same mint as
            // the stake pool
            let token_account =
                get_token_account(&config.rpc_client, value, &stake_pool.pool_mint)?;
            if token_account.mint != stake_pool.pool_mint {
                return Err("Fee receiver account belongs to a different mint"
                    .to_string()
                    .into());
            }
            *value
        }
    };

    signers.append(&mut vec![
        config.fee_payer.as_ref(),
        config.manager.as_ref(),
    ]);
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[spl_stake_pool::instruction::set_manager(
            &config.stake_pool_program_id,
            stake_pool_address,
            &config.manager.pubkey(),
            &new_manager_pubkey,
            &new_fee_receiver,
        )],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_set_staker(
    config: &Config,
    stake_pool_address: &Pubkey,
    new_staker: &Pubkey,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }
    let mut signers = vec![config.fee_payer.as_ref(), config.manager.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[spl_stake_pool::instruction::set_staker(
            &config.stake_pool_program_id,
            stake_pool_address,
            &config.manager.pubkey(),
            new_staker,
        )],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_set_funding_authority(
    config: &Config,
    stake_pool_address: &Pubkey,
    new_authority: Option<Pubkey>,
    funding_type: FundingType,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }
    let mut signers = vec![config.fee_payer.as_ref(), config.manager.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[spl_stake_pool::instruction::set_funding_authority(
            &config.stake_pool_program_id,
            stake_pool_address,
            &config.manager.pubkey(),
            new_authority.as_ref(),
            funding_type,
        )],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_set_fee(
    config: &Config,
    stake_pool_address: &Pubkey,
    new_fee: FeeType,
) -> CommandResult {
    if !config.no_update {
        command_update(config, stake_pool_address, false, false, false)?;
    }
    let mut signers = vec![config.fee_payer.as_ref(), config.manager.as_ref()];
    unique_signers!(signers);
    let transaction = checked_transaction_with_signers(
        config,
        &[spl_stake_pool::instruction::set_fee(
            &config.stake_pool_program_id,
            stake_pool_address,
            &config.manager.pubkey(),
            new_fee,
        )],
        &signers,
    )?;
    send_transaction(config, transaction)?;
    Ok(())
}

fn command_list_all_pools(config: &Config) -> CommandResult {
    let all_pools = get_stake_pools(&config.rpc_client, &config.stake_pool_program_id)?;
    let cli_stake_pool_vec: Vec<CliStakePool> =
        all_pools.into_iter().map(CliStakePool::from).collect();
    let cli_stake_pools = CliStakePools {
        pools: cli_stake_pool_vec,
    };
    println!(
        "{}",
        config.output_format.formatted_string(&cli_stake_pools)
    );
    Ok(())
}

fn main() {
    solana_logger::setup_with_default("solana=info");

    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .takes_value(false)
                .global(true)
                .help("Show additional information"),
        )
        .arg(
            Arg::with_name("output_format")
                .long("output")
                .value_name("FORMAT")
                .global(true)
                .takes_value(true)
                .possible_values(&["json", "json-compact"])
                .help("Return information in specified output format"),
        )
        .arg(
            Arg::with_name("dry_run")
                .long("dry-run")
                .takes_value(false)
                .global(true)
                .help("Simulate transaction instead of executing"),
        )
        .arg(
            Arg::with_name("no_update")
                .long("no-update")
                .takes_value(false)
                .global(true)
                .help("Do not automatically update the stake pool if needed"),
        )
        .arg(
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url)
                .global(true)
                .help("JSON RPC URL for the cluster.  Default from the configuration file."),
        )
        .arg(
            Arg::with_name("staker")
                .long("staker")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Stake pool staker. [default: cli config keypair]"),
        )
        .arg(
            Arg::with_name("manager")
                .long("manager")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Stake pool manager. [default: cli config keypair]"),
        )
        .arg(
            Arg::with_name("funding_authority")
                .long("funding-authority")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Stake pool funding authority for deposits or withdrawals. [default: cli config keypair]"),
        )
        .arg(
            Arg::with_name("token_owner")
                .long("token-owner")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Owner of pool token account [default: cli config keypair]"),
        )
        .arg(
            Arg::with_name("fee_payer")
                .long("fee-payer")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Transaction fee payer account [default: cli config keypair]"),
        )
        .arg(compute_unit_price_arg().validator(is_parsable::<u64>).global(true))
        .arg(
            Arg::with_name(COMPUTE_UNIT_LIMIT_ARG.name)
                .long(COMPUTE_UNIT_LIMIT_ARG.long)
                .takes_value(true)
                .value_name("COMPUTE-UNIT-LIMIT")
                .help(COMPUTE_UNIT_LIMIT_ARG.help)
                .validator(is_compute_unit_limit_or_simulated)
                .global(true)
        )
        .arg(
            Arg::with_name("program_id")
                .long("program-id")
                .validator(is_pubkey)
                .value_name("PROGRAM-ID")
                .takes_value(true)
                .help("Stake pool program id. [default: spl_stake_pool::id() or spl_stake_pool::devnet::id(), depending on target network]")
                .global(true)
        )
        .subcommand(SubCommand::with_name("create-pool")
            .about("Create a new stake pool")
            .arg(
                Arg::with_name("epoch_fee_numerator")
                    .long("epoch-fee-numerator")
                    .short("n")
                    .validator(is_parsable::<u64>)
                    .value_name("NUMERATOR")
                    .takes_value(true)
                    .required(true)
                    .help("Epoch fee numerator, fee amount is numerator divided by denominator."),
            )
            .arg(
                Arg::with_name("epoch_fee_denominator")
                    .long("epoch-fee-denominator")
                    .short("d")
                    .validator(is_parsable::<u64>)
                    .value_name("DENOMINATOR")
                    .takes_value(true)
                    .required(true)
                    .help("Epoch fee denominator, fee amount is numerator divided by denominator."),
            )
            .arg(
                Arg::with_name("withdrawal_fee_numerator")
                    .long("withdrawal-fee-numerator")
                    .validator(is_parsable::<u64>)
                    .value_name("NUMERATOR")
                    .takes_value(true)
                    .requires("withdrawal_fee_denominator")
                    .help("Withdrawal fee numerator, fee amount is numerator divided by denominator [default: 0]"),
            ).arg(
                Arg::with_name("withdrawal_fee_denominator")
                    .long("withdrawal-fee-denominator")
                    .validator(is_parsable::<u64>)
                    .value_name("DENOMINATOR")
                    .takes_value(true)
                    .requires("withdrawal_fee_numerator")
                    .help("Withdrawal fee denominator, fee amount is numerator divided by denominator [default: 0]"),
            )
            .arg(
                Arg::with_name("deposit_fee_numerator")
                    .long("deposit-fee-numerator")
                    .validator(is_parsable::<u64>)
                    .value_name("NUMERATOR")
                    .takes_value(true)
                    .requires("deposit_fee_denominator")
                    .help("Deposit fee numerator, fee amount is numerator divided by denominator [default: 0]"),
            ).arg(
                Arg::with_name("deposit_fee_denominator")
                    .long("deposit-fee-denominator")
                    .validator(is_parsable::<u64>)
                    .value_name("DENOMINATOR")
                    .takes_value(true)
                    .requires("deposit_fee_numerator")
                    .help("Deposit fee denominator, fee amount is numerator divided by denominator [default: 0]"),
            )
            .arg(
                Arg::with_name("referral_fee")
                    .long("referral-fee")
                    .validator(is_valid_percentage)
                    .value_name("FEE_PERCENTAGE")
                    .takes_value(true)
                    .help("Referral fee percentage, maximum 100"),
            )
            .arg(
                Arg::with_name("max_validators")
                    .long("max-validators")
                    .short("m")
                    .validator(is_parsable::<u32>)
                    .value_name("NUMBER")
                    .takes_value(true)
                    .required(true)
                    .help("Max number of validators included in the stake pool"),
            )
            .arg(
                Arg::with_name("deposit_authority")
                    .long("deposit-authority")
                    .short("a")
                    .validator(is_valid_signer)
                    .value_name("DEPOSIT_AUTHORITY_KEYPAIR")
                    .takes_value(true)
                    .help("Deposit authority required to sign all deposits into the stake pool"),
            )
            .arg(
                Arg::with_name("pool_keypair")
                    .long("pool-keypair")
                    .short("p")
                    .validator(is_keypair_or_ask_keyword)
                    .value_name("PATH")
                    .takes_value(true)
                    .help("Stake pool keypair [default: new keypair]"),
            )
            .arg(
                Arg::with_name("validator_list_keypair")
                    .long("validator-list-keypair")
                    .validator(is_keypair_or_ask_keyword)
                    .value_name("PATH")
                    .takes_value(true)
                    .help("Validator list keypair [default: new keypair]"),
            )
            .arg(
                Arg::with_name("mint_keypair")
                    .long("mint-keypair")
                    .validator(is_keypair_or_ask_keyword)
                    .value_name("PATH")
                    .takes_value(true)
                    .help("Stake pool mint keypair [default: new keypair]"),
            )
            .arg(
                Arg::with_name("reserve_keypair")
                    .long("reserve-keypair")
                    .validator(is_keypair_or_ask_keyword)
                    .value_name("PATH")
                    .takes_value(true)
                    .help("Stake pool reserve keypair [default: new keypair]"),
            )
            .arg(
                Arg::with_name("unsafe_fees")
                    .long("unsafe-fees")
                    .takes_value(false)
                    .help("Bypass fee checks, allowing pool to be created with unsafe fees"),
            )
        )
        .subcommand(SubCommand::with_name("create-token-metadata")
        .about("Creates stake pool token metadata")
        .arg(
            Arg::with_name("pool")
                .index(1)
                .validator(is_pubkey)
                .value_name("POOL_ADDRESS")
                .takes_value(true)
                .required(true)
                .help("Stake pool address"),
        )
        .arg(
            Arg::with_name("name")
                .index(2)
                .value_name("TOKEN_NAME")
                .takes_value(true)
                .required(true)
                .help("Name of the token"),
        )
        .arg(
            Arg::with_name("symbol")
                .index(3)
                .value_name("TOKEN_SYMBOL")
                .takes_value(true)
                .required(true)
                .help("Symbol of the token"),
        )
        .arg(
            Arg::with_name("uri")
                .index(4)
                .value_name("TOKEN_URI")
                .takes_value(true)
                .required(true)
                .help("URI of the token metadata json"),
        )
    )
    .subcommand(SubCommand::with_name("update-token-metadata")
    .about("Updates stake pool token metadata")
    .arg(
        Arg::with_name("pool")
            .index(1)
            .validator(is_pubkey)
            .value_name("POOL_ADDRESS")
            .takes_value(true)
            .required(true)
            .help("Stake pool address"),
    )
    .arg(
        Arg::with_name("name")
            .index(2)
            .value_name("TOKEN_NAME")
            .takes_value(true)
            .required(true)
            .help("Name of the token"),
    )
    .arg(
        Arg::with_name("symbol")
            .index(3)
            .value_name("TOKEN_SYMBOL")
            .takes_value(true)
            .required(true)
            .help("Symbol of the token"),
    )
    .arg(
        Arg::with_name("uri")
            .index(4)
            .value_name("TOKEN_URI")
            .takes_value(true)
            .required(true)
            .help("URI of the token metadata json"),
        )
    )
        .subcommand(SubCommand::with_name("add-validator")
            .about("Add validator account to the stake pool. Must be signed by the pool staker.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("vote_account")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("VOTE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("The validator vote account that the stake is delegated to"),
            )
        )
        .subcommand(SubCommand::with_name("remove-validator")
            .about("Remove validator account from the stake pool. Must be signed by the pool staker.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("vote_account")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("VOTE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Vote account for the validator to remove from the pool"),
            )
        )
        .subcommand(SubCommand::with_name("increase-validator-stake")
            .about("Increase stake to a validator, drawing from the stake pool reserve. Must be signed by the pool staker.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("vote_account")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("VOTE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Vote account for the validator to increase stake to"),
            )
            .arg(
                Arg::with_name("amount")
                    .index(3)
                    .validator(is_amount)
                    .value_name("AMOUNT")
                    .takes_value(true)
                    .help("Amount in SOL to add to the validator stake account. Must be at least the rent-exempt amount for a stake plus 1 SOL for merging."),
            )
        )
        .subcommand(SubCommand::with_name("decrease-validator-stake")
            .about("Decrease stake to a validator, splitting from the active stake. Must be signed by the pool staker.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("vote_account")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("VOTE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Vote account for the validator to decrease stake from"),
            )
            .arg(
                Arg::with_name("amount")
                    .index(3)
                    .validator(is_amount)
                    .value_name("AMOUNT")
                    .takes_value(true)
                    .help("Amount in SOL to remove from the validator stake account. Must be at least the rent-exempt amount for a stake."),
            )
        )
        .subcommand(SubCommand::with_name("set-preferred-validator")
            .about("Set the preferred validator for deposits or withdrawals. Must be signed by the pool staker.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("preferred_type")
                    .index(2)
                    .value_name("OPERATION")
                    .possible_values(&["deposit", "withdraw"]) // PreferredValidatorType enum
                    .takes_value(true)
                    .required(true)
                    .help("Operation for which to restrict the validator"),
            )
            .arg(
                Arg::with_name("vote_account")
                    .long("vote-account")
                    .validator(is_pubkey)
                    .value_name("VOTE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .help("Vote account for the validator that users must deposit into."),
            )
            .arg(
                Arg::with_name("unset")
                    .long("unset")
                    .takes_value(false)
                    .help("Unset the preferred validator."),
            )
            .group(ArgGroup::with_name("validator")
                .arg("vote_account")
                .arg("unset")
                .required(true)
            )
        )
        .subcommand(SubCommand::with_name("deposit-stake")
            .about("Deposit active stake account into the stake pool in exchange for pool tokens")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("stake_account")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("STAKE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake address to join the pool"),
            )
            .arg(
                Arg::with_name("withdraw_authority")
                    .long("withdraw-authority")
                    .validator(is_valid_signer)
                    .value_name("KEYPAIR")
                    .takes_value(true)
                    .help("Withdraw authority for the stake account to be deposited. [default: cli config keypair]"),
            )
            .arg(
                Arg::with_name("token_receiver")
                    .long("token-receiver")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Account to receive the minted pool tokens. \
                          Defaults to the token-owner's associated pool token account. \
                          Creates the account if it does not exist."),
            )
            .arg(
                Arg::with_name("referrer")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Pool token account to receive the referral fees for deposits. \
                          Defaults to the token receiver."),
            )
        )
        .subcommand(SubCommand::with_name("deposit-all-stake")
            .about("Deposit all active stake accounts into the stake pool in exchange for pool tokens")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            )
            .arg(
                Arg::with_name("stake_authority")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake authority address to search for stake accounts"),
            )
            .arg(
                Arg::with_name("withdraw_authority")
                    .long("withdraw-authority")
                    .validator(is_valid_signer)
                    .value_name("KEYPAIR")
                    .takes_value(true)
                    .help("Withdraw authority for the stake account to be deposited. [default: cli config keypair]"),
            )
            .arg(
                Arg::with_name("token_receiver")
                    .long("token-receiver")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Account to receive the minted pool tokens. \
                          Defaults to the token-owner's associated pool token account. \
                          Creates the account if it does not exist."),
            )
            .arg(
                Arg::with_name("referrer")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Pool token account to receive the referral fees for deposits. \
                          Defaults to the token receiver."),
            )
        )
        .subcommand(SubCommand::with_name("deposit-sol")
            .about("Deposit SOL into the stake pool in exchange for pool tokens")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address"),
            ).arg(
                Arg::with_name("amount")
                    .index(2)
                    .validator(is_amount)
                    .value_name("AMOUNT")
                    .takes_value(true)
                    .help("Amount in SOL to deposit into the stake pool reserve account."),
            )
            .arg(
                Arg::with_name("from")
                    .long("from")
                    .validator(is_valid_signer)
                    .value_name("KEYPAIR")
                    .takes_value(true)
                    .help("Source account of funds. [default: cli config keypair]"),
            )
            .arg(
                Arg::with_name("token_receiver")
                    .long("token-receiver")
                    .validator(is_pubkey)
                    .value_name("POOL_TOKEN_RECEIVER_ADDRESS")
                    .takes_value(true)
                    .help("Account to receive the minted pool tokens. \
                          Defaults to the token-owner's associated pool token account. \
                          Creates the account if it does not exist."),
            )
            .arg(
                Arg::with_name("referrer")
                    .long("referrer")
                    .validator(is_pubkey)
                    .value_name("REFERRER_TOKEN_ADDRESS")
                    .takes_value(true)
                    .help("Account to receive the referral fees for deposits. \
                          Defaults to the token receiver."),
            )
        )
        .subcommand(SubCommand::with_name("list")
            .about("List stake accounts managed by this pool")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
        )
        .subcommand(SubCommand::with_name("update")
            .about("Updates all balances in the pool after validator stake accounts receive rewards.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(
                Arg::with_name("force")
                    .long("force")
                    .takes_value(false)
                    .help("Update balances, even if it has already been performed this epoch."),
            )
            .arg(
                Arg::with_name("no_merge")
                    .long("no-merge")
                    .takes_value(false)
                    .help("Do not automatically merge transient stakes. Useful if the stake pool is in an expected state, but the balances still need to be updated."),
            )
            .arg(
                Arg::with_name("stale_only")
                    .long("stale-only")
                    .takes_value(false)
                    .help("If set, only updates validator list balances that have not been updated for this epoch. Otherwise, updates all validator balances on the validator list."),
            )
        )
        .subcommand(SubCommand::with_name("withdraw-stake")
            .about("Withdraw active stake from the stake pool in exchange for pool tokens")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(
                Arg::with_name("amount")
                    .index(2)
                    .validator(is_amount)
                    .value_name("AMOUNT")
                    .takes_value(true)
                    .required(true)
                    .help("Amount of pool tokens to withdraw for activated stake."),
            )
            .arg(
                Arg::with_name("pool_account")
                    .long("pool-account")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Pool token account to withdraw tokens from. Defaults to the token-owner's associated token account."),
            )
            .arg(
                Arg::with_name("stake_receiver")
                    .long("stake-receiver")
                    .validator(is_pubkey)
                    .value_name("STAKE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .requires("withdraw_from")
                    .help("Stake account from which to receive a stake from the stake pool. Defaults to a new stake account."),
            )
            .arg(
                Arg::with_name("vote_account")
                    .long("vote-account")
                    .validator(is_pubkey)
                    .value_name("VOTE_ACCOUNT_ADDRESS")
                    .takes_value(true)
                    .help("Validator to withdraw from. Defaults to the largest validator stakes in the pool."),
            )
            .arg(
                Arg::with_name("use_reserve")
                    .long("use-reserve")
                    .takes_value(false)
                    .help("Withdraw from the stake pool's reserve. Only possible if all validator stakes are at the minimum possible amount."),
            )
            .group(ArgGroup::with_name("withdraw_from")
                .arg("use_reserve")
                .arg("vote_account")
            )
        )
        .subcommand(SubCommand::with_name("withdraw-sol")
            .about("Withdraw SOL from the stake pool's reserve in exchange for pool tokens")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(
                Arg::with_name("sol_receiver")
                    .index(2)
                    .validator(is_valid_pubkey)
                    .value_name("SYSTEM_ACCOUNT_ADDRESS_OR_KEYPAIR")
                    .takes_value(true)
                    .required(true)
                    .help("System account to receive SOL from the stake pool. Defaults to the payer."),
            )
            .arg(
                Arg::with_name("amount")
                    .index(3)
                    .validator(is_amount)
                    .value_name("AMOUNT")
                    .takes_value(true)
                    .required(true)
                    .help("Amount of pool tokens to withdraw for SOL."),
            )
            .arg(
                Arg::with_name("pool_account")
                    .long("pool-account")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Pool token account to withdraw tokens from. Defaults to the token-owner's associated token account."),
            )
        )
        .subcommand(SubCommand::with_name("set-manager")
            .about("Change manager or fee receiver account for the stake pool. Must be signed by the current manager.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(
                Arg::with_name("new_manager")
                    .long("new-manager")
                    .validator(is_valid_signer)
                    .value_name("KEYPAIR")
                    .takes_value(true)
                    .help("Keypair for the new stake pool manager."),
            )
            .arg(
                Arg::with_name("new_fee_receiver")
                    .long("new-fee-receiver")
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Public key for the new account to set as the stake pool fee receiver."),
            )
            .group(ArgGroup::with_name("new_accounts")
                .arg("new_manager")
                .arg("new_fee_receiver")
                .required(true)
                .multiple(true)
            )
        )
        .subcommand(SubCommand::with_name("set-staker")
            .about("Change staker account for the stake pool. Must be signed by the manager or current staker.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(
                Arg::with_name("new_staker")
                    .index(2)
                    .validator(is_pubkey)
                    .value_name("ADDRESS")
                    .takes_value(true)
                    .help("Public key for the new stake pool staker."),
            )
        )
        .subcommand(SubCommand::with_name("set-funding-authority")
            .about("Change one of the funding authorities for the stake pool. Must be signed by the manager.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(
                Arg::with_name("funding_type")
                    .index(2)
                    .value_name("FUNDING_TYPE")
                    .possible_values(&["stake-deposit", "sol-deposit", "sol-withdraw"]) // FundingType enum
                    .takes_value(true)
                    .required(true)
                    .help("Funding type to be updated."),
            )
            .arg(
                Arg::with_name("new_authority")
                    .index(3)
                    .validator(is_pubkey)
                    .value_name("AUTHORITY_ADDRESS")
                    .takes_value(true)
                    .help("Public key for the new stake pool funding authority."),
            )
            .arg(
                Arg::with_name("unset")
                    .long("unset")
                    .takes_value(false)
                    .help("Unset the stake deposit authority. The program will use a program derived address.")
            )
            .group(ArgGroup::with_name("validator")
                .arg("new_authority")
                .arg("unset")
                .required(true)
            )
        )
        .subcommand(SubCommand::with_name("set-fee")
            .about("Change the [epoch/withdraw/stake deposit/sol deposit] fee assessed by the stake pool. Must be signed by the manager.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(Arg::with_name("fee_type")
                .index(2)
                .value_name("FEE_TYPE")
                .possible_values(&["epoch", "stake-deposit", "sol-deposit", "stake-withdrawal", "sol-withdrawal"]) // FeeType enum
                .takes_value(true)
                .required(true)
                .help("Fee type to be updated."),
            )
            .arg(
                Arg::with_name("fee_numerator")
                    .index(3)
                    .validator(is_parsable::<u64>)
                    .value_name("NUMERATOR")
                    .takes_value(true)
                    .required(true)
                    .help("Fee numerator, fee amount is numerator divided by denominator."),
            )
            .arg(
                Arg::with_name("fee_denominator")
                    .index(4)
                    .validator(is_parsable::<u64>)
                    .value_name("DENOMINATOR")
                    .takes_value(true)
                    .required(true)
                    .help("Fee denominator, fee amount is numerator divided by denominator."),
            )
        )
        .subcommand(SubCommand::with_name("set-referral-fee")
            .about("Change the referral fee assessed by the stake pool for stake deposits. Must be signed by the manager.")
            .arg(
                Arg::with_name("pool")
                    .index(1)
                    .validator(is_pubkey)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .help("Stake pool address."),
            )
            .arg(Arg::with_name("fee_type")
                .index(2)
                .value_name("FEE_TYPE")
                .possible_values(&["stake", "sol"]) // FeeType enum, kind of
                .takes_value(true)
                .required(true)
                .help("Fee type to be updated."),
            )
            .arg(
                Arg::with_name("fee")
                    .index(3)
                    .validator(is_valid_percentage)
                    .value_name("FEE_PERCENTAGE")
                    .takes_value(true)
                    .required(true)
                    .help("Fee percentage, maximum 100"),
            )
        )
        .subcommand(SubCommand::with_name("list-all")
            .about("List information about all stake pools")
        )
        .get_matches();

    let mut wallet_manager = None;
    let cli_config = if let Some(config_file) = matches.value_of("config_file") {
        solana_cli_config::Config::load(config_file).unwrap_or_default()
    } else {
        solana_cli_config::Config::default()
    };
    let config = {
        let json_rpc_url = value_t!(matches, "json_rpc_url", String)
            .unwrap_or_else(|_| cli_config.json_rpc_url.clone());

        let staker = get_signer(
            &matches,
            "staker",
            &cli_config.keypair_path,
            &mut wallet_manager,
            SignerFromPathConfig {
                allow_null_signer: false,
            },
        );

        let funding_authority = if matches.is_present("funding_authority") {
            Some(get_signer(
                &matches,
                "funding_authority",
                &cli_config.keypair_path,
                &mut wallet_manager,
                SignerFromPathConfig {
                    allow_null_signer: false,
                },
            ))
        } else {
            None
        };
        let manager = get_signer(
            &matches,
            "manager",
            &cli_config.keypair_path,
            &mut wallet_manager,
            SignerFromPathConfig {
                allow_null_signer: false,
            },
        );
        let token_owner = get_signer(
            &matches,
            "token_owner",
            &cli_config.keypair_path,
            &mut wallet_manager,
            SignerFromPathConfig {
                allow_null_signer: false,
            },
        );
        let fee_payer = get_signer(
            &matches,
            "fee_payer",
            &cli_config.keypair_path,
            &mut wallet_manager,
            SignerFromPathConfig {
                allow_null_signer: false,
            },
        );
        let verbose = matches.is_present("verbose");
        let stake_pool_program_id = pubkey_of(&matches, "program_id")
            .unwrap_or_else(|| default_stake_pool_id(&json_rpc_url));
        let output_format = matches
            .value_of("output_format")
            .map(|value| match value {
                "json" => OutputFormat::Json,
                "json-compact" => OutputFormat::JsonCompact,
                _ => unreachable!(),
            })
            .unwrap_or(if verbose {
                OutputFormat::DisplayVerbose
            } else {
                OutputFormat::Display
            });
        let dry_run = matches.is_present("dry_run");
        let no_update = matches.is_present("no_update");
        let compute_unit_price = value_t!(matches, COMPUTE_UNIT_PRICE_ARG.name, u64).ok();
        let compute_unit_limit = matches
            .value_of(COMPUTE_UNIT_LIMIT_ARG.name)
            .map(|x| parse_compute_unit_limit(x).unwrap())
            .unwrap_or_else(|| {
                if compute_unit_price.is_some() {
                    ComputeUnitLimit::Simulated
                } else {
                    ComputeUnitLimit::Default
                }
            });

        Config {
            rpc_client: RpcClient::new_with_commitment(
                &json_rpc_url,
                CommitmentConfig::confirmed(),
            ),
            stake_pool_program_id,
            verbose,
            output_format,
            manager,
            staker,
            funding_authority,
            token_owner,
            fee_payer,
            dry_run,
            no_update,
            compute_unit_price,
            compute_unit_limit,
        }
    };

    let _ = match matches.subcommand() {
        ("create-pool", Some(arg_matches)) => {
            let deposit_authority = keypair_of(arg_matches, "deposit_authority");
            let e_numerator = value_t_or_exit!(arg_matches, "epoch_fee_numerator", u64);
            let e_denominator = value_t_or_exit!(arg_matches, "epoch_fee_denominator", u64);
            let w_numerator = value_t!(arg_matches, "withdrawal_fee_numerator", u64);
            let w_denominator = value_t!(arg_matches, "withdrawal_fee_denominator", u64);
            let d_numerator = value_t!(arg_matches, "deposit_fee_numerator", u64);
            let d_denominator = value_t!(arg_matches, "deposit_fee_denominator", u64);
            let referral_fee = value_t!(arg_matches, "referral_fee", u8);
            let max_validators = value_t_or_exit!(arg_matches, "max_validators", u32);
            let pool_keypair = keypair_of(arg_matches, "pool_keypair");
            let validator_list_keypair = keypair_of(arg_matches, "validator_list_keypair");
            let mint_keypair = keypair_of(arg_matches, "mint_keypair");
            let reserve_keypair = keypair_of(arg_matches, "reserve_keypair");
            let unsafe_fees = arg_matches.is_present("unsafe_fees");
            command_create_pool(
                &config,
                deposit_authority,
                Fee {
                    numerator: e_numerator,
                    denominator: e_denominator,
                },
                Fee {
                    numerator: w_numerator.unwrap_or(0),
                    denominator: w_denominator.unwrap_or(0),
                },
                Fee {
                    numerator: d_numerator.unwrap_or(0),
                    denominator: d_denominator.unwrap_or(0),
                },
                referral_fee.unwrap_or(0),
                max_validators,
                pool_keypair,
                validator_list_keypair,
                mint_keypair,
                reserve_keypair,
                unsafe_fees,
            )
        }
        ("create-token-metadata", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let name = value_t_or_exit!(arg_matches, "name", String);
            let symbol = value_t_or_exit!(arg_matches, "symbol", String);
            let uri = value_t_or_exit!(arg_matches, "uri", String);
            create_token_metadata(&config, &stake_pool_address, name, symbol, uri)
        }
        ("update-token-metadata", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let name = value_t_or_exit!(arg_matches, "name", String);
            let symbol = value_t_or_exit!(arg_matches, "symbol", String);
            let uri = value_t_or_exit!(arg_matches, "uri", String);
            update_token_metadata(&config, &stake_pool_address, name, symbol, uri)
        }
        ("add-validator", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let vote_account_address = pubkey_of(arg_matches, "vote_account").unwrap();
            command_vsa_add(&config, &stake_pool_address, &vote_account_address)
        }
        ("remove-validator", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let vote_account = pubkey_of(arg_matches, "vote_account").unwrap();
            command_vsa_remove(&config, &stake_pool_address, &vote_account)
        }
        ("increase-validator-stake", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let vote_account = pubkey_of(arg_matches, "vote_account").unwrap();
            let amount_str = arg_matches.value_of("amount").unwrap();
            let lamports = native_token::sol_str_to_lamports(amount_str).unwrap();
            command_increase_validator_stake(&config, &stake_pool_address, &vote_account, lamports)
        }
        ("decrease-validator-stake", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let vote_account = pubkey_of(arg_matches, "vote_account").unwrap();
            let amount_str = arg_matches.value_of("amount").unwrap();
            let lamports = native_token::sol_str_to_lamports(amount_str).unwrap();
            command_decrease_validator_stake(&config, &stake_pool_address, &vote_account, lamports)
        }
        ("set-preferred-validator", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let preferred_type = match arg_matches.value_of("preferred_type").unwrap() {
                "deposit" => PreferredValidatorType::Deposit,
                "withdraw" => PreferredValidatorType::Withdraw,
                _ => unreachable!(),
            };
            let vote_account = pubkey_of(arg_matches, "vote_account");
            let _unset = arg_matches.is_present("unset");
            // since unset and vote_account can't both be set, if unset is set
            // then vote_account will be None, which is valid for the program
            command_set_preferred_validator(
                &config,
                &stake_pool_address,
                preferred_type,
                vote_account,
            )
        }
        ("deposit-stake", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let stake_account = pubkey_of(arg_matches, "stake_account").unwrap();
            let token_receiver: Option<Pubkey> = pubkey_of(arg_matches, "token_receiver");
            let referrer: Option<Pubkey> = pubkey_of(arg_matches, "referrer");
            let withdraw_authority = get_signer(
                arg_matches,
                "withdraw_authority",
                &cli_config.keypair_path,
                &mut wallet_manager,
                SignerFromPathConfig {
                    allow_null_signer: false,
                },
            );
            command_deposit_stake(
                &config,
                &stake_pool_address,
                &stake_account,
                withdraw_authority,
                &token_receiver,
                &referrer,
            )
        }
        ("deposit-sol", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let token_receiver: Option<Pubkey> = pubkey_of(arg_matches, "token_receiver");
            let referrer: Option<Pubkey> = pubkey_of(arg_matches, "referrer");
            let from = keypair_of(arg_matches, "from");
            let amount_str = arg_matches.value_of("amount").unwrap();
            let lamports = native_token::sol_str_to_lamports(amount_str).unwrap();
            command_deposit_sol(
                &config,
                &stake_pool_address,
                &from,
                &token_receiver,
                &referrer,
                lamports,
            )
        }
        ("list", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            command_list(&config, &stake_pool_address)
        }
        ("update", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let no_merge = arg_matches.is_present("no_merge");
            let force = arg_matches.is_present("force");
            let stale_only = arg_matches.is_present("stale_only");
            command_update(&config, &stake_pool_address, force, no_merge, stale_only)
        }
        ("withdraw-stake", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let vote_account = pubkey_of(arg_matches, "vote_account");
            let pool_account = pubkey_of(arg_matches, "pool_account");
            let pool_amount = value_t_or_exit!(arg_matches, "amount", f64);
            let stake_receiver = pubkey_of(arg_matches, "stake_receiver");
            let use_reserve = arg_matches.is_present("use_reserve");
            command_withdraw_stake(
                &config,
                &stake_pool_address,
                use_reserve,
                &vote_account,
                &stake_receiver,
                &pool_account,
                pool_amount,
            )
        }
        ("withdraw-sol", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let pool_account = pubkey_of(arg_matches, "pool_account");
            let pool_amount = value_t_or_exit!(arg_matches, "amount", f64);
            let sol_receiver = get_signer(
                arg_matches,
                "sol_receiver",
                &cli_config.keypair_path,
                &mut wallet_manager,
                SignerFromPathConfig {
                    allow_null_signer: true,
                },
            )
            .pubkey();
            command_withdraw_sol(
                &config,
                &stake_pool_address,
                &pool_account,
                &sol_receiver,
                pool_amount,
            )
        }
        ("set-manager", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();

            let new_manager = if arg_matches.value_of("new_manager").is_some() {
                let signer = get_signer(
                    arg_matches,
                    "new-manager",
                    arg_matches
                        .value_of("new_manager")
                        .expect("new manager argument not found!"),
                    &mut wallet_manager,
                    SignerFromPathConfig {
                        allow_null_signer: true,
                    },
                );
                Some(signer)
            } else {
                None
            };

            let new_fee_receiver: Option<Pubkey> = pubkey_of(arg_matches, "new_fee_receiver");
            command_set_manager(
                &config,
                &stake_pool_address,
                &new_manager,
                &new_fee_receiver,
            )
        }
        ("set-staker", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let new_staker = pubkey_of(arg_matches, "new_staker").unwrap();
            command_set_staker(&config, &stake_pool_address, &new_staker)
        }
        ("set-funding-authority", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let new_authority = pubkey_of(arg_matches, "new_authority");
            let funding_type = match arg_matches.value_of("funding_type").unwrap() {
                "sol-deposit" => FundingType::SolDeposit,
                "stake-deposit" => FundingType::StakeDeposit,
                "sol-withdraw" => FundingType::SolWithdraw,
                _ => unreachable!(),
            };
            let _unset = arg_matches.is_present("unset");
            command_set_funding_authority(&config, &stake_pool_address, new_authority, funding_type)
        }
        ("set-fee", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let numerator = value_t_or_exit!(arg_matches, "fee_numerator", u64);
            let denominator = value_t_or_exit!(arg_matches, "fee_denominator", u64);
            let new_fee = Fee {
                denominator,
                numerator,
            };
            match arg_matches.value_of("fee_type").unwrap() {
                "epoch" => command_set_fee(&config, &stake_pool_address, FeeType::Epoch(new_fee)),
                "stake-deposit" => {
                    command_set_fee(&config, &stake_pool_address, FeeType::StakeDeposit(new_fee))
                }
                "sol-deposit" => {
                    command_set_fee(&config, &stake_pool_address, FeeType::SolDeposit(new_fee))
                }
                "stake-withdrawal" => command_set_fee(
                    &config,
                    &stake_pool_address,
                    FeeType::StakeWithdrawal(new_fee),
                ),
                "sol-withdrawal" => command_set_fee(
                    &config,
                    &stake_pool_address,
                    FeeType::SolWithdrawal(new_fee),
                ),
                _ => unreachable!(),
            }
        }
        ("set-referral-fee", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let fee = value_t_or_exit!(arg_matches, "fee", u8);
            assert!(
                fee <= 100u8,
                "Invalid fee {}%. Fee needs to be in range [0-100]",
                fee
            );
            let fee_type = match arg_matches.value_of("fee_type").unwrap() {
                "sol" => FeeType::SolReferral(fee),
                "stake" => FeeType::StakeReferral(fee),
                _ => unreachable!(),
            };
            command_set_fee(&config, &stake_pool_address, fee_type)
        }
        ("list-all", _) => command_list_all_pools(&config),
        ("deposit-all-stake", Some(arg_matches)) => {
            let stake_pool_address = pubkey_of(arg_matches, "pool").unwrap();
            let stake_authority = pubkey_of(arg_matches, "stake_authority").unwrap();
            let token_receiver: Option<Pubkey> = pubkey_of(arg_matches, "token_receiver");
            let referrer: Option<Pubkey> = pubkey_of(arg_matches, "referrer");
            let withdraw_authority = get_signer(
                arg_matches,
                "withdraw_authority",
                &cli_config.keypair_path,
                &mut wallet_manager,
                SignerFromPathConfig {
                    allow_null_signer: false,
                },
            );
            command_deposit_all_stake(
                &config,
                &stake_pool_address,
                &stake_authority,
                withdraw_authority,
                &token_receiver,
                &referrer,
            )
        }
        _ => unreachable!(),
    }
    .map_err(|err| {
        eprintln!("{}", err);
        exit(1);
    });
}
