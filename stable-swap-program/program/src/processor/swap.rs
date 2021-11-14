//! Module for processing non-admin pool instructions.

use crate::{
    curve::{StableSwap, MAX_AMP, MIN_AMP, ZERO_TS},
    error::SwapError,
    fees::Fees,
    instruction::{
        DepositData, InitializeData, SwapData, SwapInstruction, WithdrawData, WithdrawOneData,
    },
    pool_converter::PoolTokenConverter,
    processor::utils,
    state::{SwapInfo, SwapTokenInfo},
};

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program_error::ProgramError,
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    sysvar::{clock::Clock, Sysvar},
};

use super::checks::*;
use super::logging::*;
use super::token;

pub fn process_swap_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let instruction = SwapInstruction::unpack(input)?;
    match instruction {
        SwapInstruction::Initialize(InitializeData {
            nonce,
            amp_factor,
            fees,
        }) => {
            msg!("Instruction: Init");
            process_initialize(program_id, nonce, amp_factor, fees, accounts)
        }
        SwapInstruction::Swap(SwapData {
            amount_in,
            minimum_amount_out,
        }) => {
            msg!("Instruction: Swap");
            process_swap(program_id, amount_in, minimum_amount_out, accounts)
        }
        SwapInstruction::Deposit(DepositData {
            token_a_amount,
            token_b_amount,
            min_mint_amount,
        }) => {
            msg!("Instruction: Deposit");
            process_deposit(
                program_id,
                token_a_amount,
                token_b_amount,
                min_mint_amount,
                accounts,
            )
        }
        SwapInstruction::Withdraw(WithdrawData {
            pool_token_amount,
            minimum_token_a_amount,
            minimum_token_b_amount,
        }) => {
            msg!("Instruction: Withdraw");
            process_withdraw(
                program_id,
                pool_token_amount,
                minimum_token_a_amount,
                minimum_token_b_amount,
                accounts,
            )
        }
        SwapInstruction::WithdrawOne(WithdrawOneData {
            pool_token_amount,
            minimum_token_amount,
        }) => {
            msg!("Instruction: Withdraw One");
            process_withdraw_one(
                program_id,
                pool_token_amount,
                minimum_token_amount,
                accounts,
            )
        }
    }
}

/// Processes an [Initialize](enum.Instruction.html).
fn process_initialize(
    program_id: &Pubkey,
    nonce: u8,
    amp_factor: u64,
    fees: Fees,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let swap_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let admin_key_info = next_account_info(account_info_iter)?;
    let admin_fee_a_info = next_account_info(account_info_iter)?;
    let admin_fee_b_info = next_account_info(account_info_iter)?;
    let token_a_mint_info = next_account_info(account_info_iter)?;
    let token_a_info = next_account_info(account_info_iter)?;
    let token_b_mint_info = next_account_info(account_info_iter)?;
    let token_b_info = next_account_info(account_info_iter)?;
    let pool_mint_info = next_account_info(account_info_iter)?;
    let destination_info = next_account_info(account_info_iter)?; // Destination account to mint LP tokens to
    let token_program_info = next_account_info(account_info_iter)?;
    let clock_sysvar_info = next_account_info(account_info_iter)?;

    if !(MIN_AMP..=MAX_AMP).contains(&amp_factor) {
        msg!("Invalid amp factor: {}", amp_factor);
        return Err(SwapError::InvalidInput.into());
    }

    let token_swap = SwapInfo::unpack_unchecked(&swap_info.data.borrow())?;
    if token_swap.is_initialized {
        return Err(SwapError::AlreadyInUse.into());
    }
    let swap_authority = utils::authority_id(program_id, swap_info.key, nonce)?;
    check_keys_equal!(
        *authority_info.key,
        swap_authority,
        "Swap authority",
        SwapError::InvalidProgramAddress
    );

    let destination = utils::unpack_token_account(&destination_info.data.borrow())?;
    let token_a = utils::unpack_token_account(&token_a_info.data.borrow())?;
    let token_b = utils::unpack_token_account(&token_b_info.data.borrow())?;

    check_keys_equal!(
        *authority_info.key,
        token_a.owner,
        "Token A authority",
        SwapError::InvalidOwner
    );
    check_keys_equal!(
        *authority_info.key,
        token_b.owner,
        "Token B authority",
        SwapError::InvalidOwner
    );
    check_keys_not_equal!(
        *authority_info.key,
        destination.owner,
        "Initial LP destination authority",
        SwapError::InvalidOutputOwner
    );

    if token_a.mint == token_b.mint {
        return Err(SwapError::RepeatedMint.into());
    }
    if token_b.amount == 0 {
        return Err(SwapError::EmptySupply.into());
    }
    if token_a.amount == 0 {
        return Err(SwapError::EmptySupply.into());
    }
    if token_a.delegate.is_some() {
        return Err(SwapError::InvalidDelegate.into());
    }
    if token_b.delegate.is_some() {
        return Err(SwapError::InvalidDelegate.into());
    }
    check_keys_equal!(
        token_a.mint,
        *token_a_mint_info.key,
        "Mint A",
        SwapError::IncorrectMint
    );
    check_keys_equal!(
        token_b.mint,
        *token_b_mint_info.key,
        "Mint B",
        SwapError::IncorrectMint
    );
    if token_a.close_authority.is_some() {
        return Err(SwapError::InvalidCloseAuthority.into());
    }
    if token_b.close_authority.is_some() {
        return Err(SwapError::InvalidCloseAuthority.into());
    }
    let pool_mint = utils::unpack_mint(&pool_mint_info.data.borrow())?;
    check_keys_equal_optional!(
        pool_mint.mint_authority,
        COption::Some(*authority_info.key),
        "LP mint authority",
        SwapError::InvalidOwner
    );
    if pool_mint.freeze_authority.is_some() {
        return Err(SwapError::InvalidFreezeAuthority.into());
    }
    if pool_mint.supply != 0 {
        return Err(SwapError::InvalidSupply.into());
    }
    let token_a_mint = utils::unpack_mint(&token_a_mint_info.data.borrow())?;
    let token_b_mint = utils::unpack_mint(&token_b_mint_info.data.borrow())?;
    if token_a_mint.decimals != token_b_mint.decimals {
        return Err(SwapError::MismatchedDecimals.into());
    }
    if pool_mint.decimals != token_a_mint.decimals {
        return Err(SwapError::MismatchedDecimals.into());
    }
    let admin_fee_key_a = utils::unpack_token_account(&admin_fee_a_info.data.borrow())?;
    let admin_fee_key_b = utils::unpack_token_account(&admin_fee_b_info.data.borrow())?;

    check_keys_equal!(
        token_a.mint,
        admin_fee_key_a.mint,
        "Mint A",
        SwapError::InvalidAdmin
    );
    check_keys_equal!(
        token_b.mint,
        admin_fee_key_b.mint,
        "Mint B",
        SwapError::InvalidAdmin
    );

    // amp_factor == initial_amp_factor == target_amp_factor on init
    let invariant = StableSwap::new(amp_factor, amp_factor, ZERO_TS, ZERO_TS, ZERO_TS);
    // Compute amount of LP tokens to mint for bootstrapper
    let mint_amount_u256 = invariant
        .compute_d(token_a.amount, token_b.amount)
        .ok_or(SwapError::CalculationFailure)?;
    let mint_amount = (mint_amount_u256.try_to_u64())?;
    token::mint_to(
        swap_info.key,
        token_program_info.clone(),
        pool_mint_info.clone(),
        destination_info.clone(),
        authority_info.clone(),
        nonce,
        mint_amount,
    )?;

    let obj = SwapInfo {
        is_initialized: true,
        is_paused: false,
        nonce,
        initial_amp_factor: amp_factor,
        target_amp_factor: amp_factor,
        start_ramp_ts: ZERO_TS,
        stop_ramp_ts: ZERO_TS,
        future_admin_deadline: ZERO_TS,
        future_admin_key: Pubkey::default(),
        admin_key: *admin_key_info.key,
        token_a: SwapTokenInfo {
            reserves: *token_a_info.key,
            mint: token_a.mint,
            admin_fees: *admin_fee_a_info.key,
            index: 0,
        },
        token_b: SwapTokenInfo {
            reserves: *token_b_info.key,
            mint: token_b.mint,
            admin_fees: *admin_fee_b_info.key,
            index: 1,
        },
        pool_mint: *pool_mint_info.key,
        fees,
    };
    SwapInfo::pack(obj, &mut swap_info.data.borrow_mut())?;

    let clock = Clock::from_account_info(clock_sysvar_info)?;
    log_event(
        Event::Deposit,
        clock.unix_timestamp,
        token_a.amount,
        token_b.amount,
        mint_amount,
        0,
    );

    Ok(())
}

/// Processes an [Swap](enum.Instruction.html).
fn process_swap(
    program_id: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if amount_in == 0 {
        // noop
        return Ok(());
    }
    let account_info_iter = &mut accounts.iter();
    let swap_info = next_account_info(account_info_iter)?;
    let swap_authority_info = next_account_info(account_info_iter)?;
    let user_authority_info = next_account_info(account_info_iter)?;
    let source_info = next_account_info(account_info_iter)?;
    let swap_source_info = next_account_info(account_info_iter)?;
    let swap_destination_info = next_account_info(account_info_iter)?;
    let destination_info = next_account_info(account_info_iter)?;
    let admin_destination_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let clock_sysvar_info = next_account_info(account_info_iter)?;

    if *swap_source_info.key == *swap_destination_info.key {
        return Err(SwapError::InvalidInput.into());
    }

    let token_swap = SwapInfo::unpack(&swap_info.data.borrow())?;
    if token_swap.is_paused {
        return Err(SwapError::IsPaused.into());
    }

    check_token_keys_not_equal!(
        token_swap.token_a,
        *source_info.key,
        token_swap.token_a.reserves,
        "Source account cannot be one of swap's token accounts for token",
        SwapError::InvalidInput
    );

    check_token_keys_not_equal!(
        token_swap.token_b,
        *source_info.key,
        token_swap.token_b.reserves,
        "Source account cannot be one of swap's token accounts for token",
        SwapError::InvalidInput
    );

    check_swap_authority(
        &token_swap,
        swap_info.key,
        program_id,
        swap_authority_info.key,
    )?;

    if *swap_source_info.key == token_swap.token_a.reserves {
        // Swap A to B
        check_swap_token_destination_accounts(
            &token_swap.token_b,
            swap_destination_info.key,
            admin_destination_info.key,
        )?;
    } else if *swap_source_info.key == token_swap.token_b.reserves {
        // Swap B to A
        check_swap_token_destination_accounts(
            &token_swap.token_a,
            swap_destination_info.key,
            admin_destination_info.key,
        )?;
    } else {
        return Err(SwapError::IncorrectSwapAccount.into());
    }

    let clock = Clock::from_account_info(clock_sysvar_info)?;
    let swap_source_account = utils::unpack_token_account(&swap_source_info.data.borrow())?;
    let swap_destination_account =
        utils::unpack_token_account(&swap_destination_info.data.borrow())?;

    let invariant = StableSwap::new(
        token_swap.initial_amp_factor,
        token_swap.target_amp_factor,
        clock.unix_timestamp,
        token_swap.start_ramp_ts,
        token_swap.stop_ramp_ts,
    );
    let result = invariant
        .swap_to(
            amount_in,
            swap_source_account.amount,
            swap_destination_account.amount,
            &token_swap.fees,
        )
        .ok_or(SwapError::CalculationFailure)?;
    let amount_swapped = result.amount_swapped;
    if amount_swapped < minimum_amount_out {
        log_slippage_error(minimum_amount_out, amount_swapped);
        return Err(SwapError::ExceededSlippage.into());
    }

    // from user to swap
    token::transfer_as_user(
        token_program_info.clone(),
        source_info.clone(),
        swap_source_info.clone(),
        user_authority_info.clone(),
        amount_in,
    )?;
    // from swap to user
    token::transfer_as_swap(
        swap_info.key,
        token_program_info.clone(),
        swap_destination_info.clone(),
        destination_info.clone(),
        swap_authority_info.clone(),
        token_swap.nonce,
        amount_swapped,
    )?;
    // from swap to fees
    token::transfer_as_swap(
        swap_info.key,
        token_program_info.clone(),
        swap_destination_info.clone(),
        admin_destination_info.clone(),
        swap_authority_info.clone(),
        token_swap.nonce,
        result.admin_fee,
    )?;

    if *swap_source_info.key == token_swap.token_a.reserves {
        log_event(
            Event::SwapAToB,
            clock.unix_timestamp,
            amount_in,
            amount_swapped,
            0,
            result.fee,
        );
    } else {
        log_event(
            Event::SwapBToA,
            clock.unix_timestamp,
            amount_swapped,
            amount_in,
            0,
            result.fee,
        );
    };

    Ok(())
}

/// Processes an [Deposit](enum.Instruction.html).
fn process_deposit(
    program_id: &Pubkey,
    token_a_amount: u64,
    token_b_amount: u64,
    min_mint_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if token_a_amount == 0 && token_b_amount == 0 {
        // noop
        return Ok(());
    }
    let account_info_iter = &mut accounts.iter();
    let swap_info = next_account_info(account_info_iter)?;
    let swap_authority_info = next_account_info(account_info_iter)?;
    let user_authority_info = next_account_info(account_info_iter)?;
    let source_a_info = next_account_info(account_info_iter)?;
    let source_b_info = next_account_info(account_info_iter)?;
    let token_a_info = next_account_info(account_info_iter)?;
    let token_b_info = next_account_info(account_info_iter)?;
    let pool_mint_info = next_account_info(account_info_iter)?;
    let dest_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let clock_sysvar_info = next_account_info(account_info_iter)?;

    let token_swap = SwapInfo::unpack(&swap_info.data.borrow())?;
    if token_swap.is_paused {
        return Err(SwapError::IsPaused.into());
    }
    check_swap_authority(
        &token_swap,
        swap_info.key,
        program_id,
        swap_authority_info.key,
    )?;

    check_deposit_token_accounts(&token_swap.token_a, source_a_info.key, token_a_info.key)?;
    check_deposit_token_accounts(&token_swap.token_b, source_b_info.key, token_b_info.key)?;

    check_keys_equal!(
        *pool_mint_info.key,
        token_swap.pool_mint,
        "Mint A",
        SwapError::IncorrectMint
    );

    let clock = Clock::from_account_info(clock_sysvar_info)?;
    let token_a = utils::unpack_token_account(&token_a_info.data.borrow())?;
    let token_b = utils::unpack_token_account(&token_b_info.data.borrow())?;
    let pool_mint = utils::unpack_mint(&pool_mint_info.data.borrow())?;

    let invariant = StableSwap::new(
        token_swap.initial_amp_factor,
        token_swap.target_amp_factor,
        clock.unix_timestamp,
        token_swap.start_ramp_ts,
        token_swap.stop_ramp_ts,
    );
    let mint_amount = invariant
        .compute_mint_amount_for_deposit(
            token_a_amount,
            token_b_amount,
            token_a.amount,
            token_b.amount,
            pool_mint.supply,
            &token_swap.fees,
        )
        .ok_or(SwapError::CalculationFailure)?;
    if mint_amount < min_mint_amount {
        log_slippage_error(min_mint_amount, mint_amount);
        return Err(SwapError::ExceededSlippage.into());
    }

    // from user to swap
    token::transfer_as_user(
        token_program_info.clone(),
        source_a_info.clone(),
        token_a_info.clone(),
        user_authority_info.clone(),
        token_a_amount,
    )?;
    // from user to swap
    token::transfer_as_user(
        token_program_info.clone(),
        source_b_info.clone(),
        token_b_info.clone(),
        user_authority_info.clone(),
        token_b_amount,
    )?;
    // mint lp to user
    token::mint_to(
        swap_info.key,
        token_program_info.clone(),
        pool_mint_info.clone(),
        dest_info.clone(),
        swap_authority_info.clone(),
        token_swap.nonce,
        mint_amount,
    )?;

    log_event(
        Event::Deposit,
        clock.unix_timestamp,
        token_a_amount,
        token_b_amount,
        mint_amount,
        0,
    );

    Ok(())
}

struct WithdrawContext<'a, 'b: 'a> {
    token_swap: SwapInfo,
    token_program_info: &'a AccountInfo<'b>,
    swap_authority_info: &'a AccountInfo<'b>,
    swap_info: &'a AccountInfo<'b>,
}

fn handle_token_withdraw<'a, 'b: 'a>(
    ctx: &WithdrawContext<'a, 'b>,
    (amount, admin_fee): (u64, u64),
    reserves_info: &'a AccountInfo<'b>,
    dest_token_info: &'a AccountInfo<'b>,
    admin_fee_dest_info: &'a AccountInfo<'b>,
) -> ProgramResult {
    // from swap to user
    token::transfer_as_swap(
        ctx.swap_info.key,
        ctx.token_program_info.clone(),
        reserves_info.clone(),
        dest_token_info.clone(),
        ctx.swap_authority_info.clone(),
        ctx.token_swap.nonce,
        amount,
    )?;
    // from swap to fee
    token::transfer_as_swap(
        ctx.swap_info.key,
        ctx.token_program_info.clone(),
        reserves_info.clone(),
        admin_fee_dest_info.clone(),
        ctx.swap_authority_info.clone(),
        ctx.token_swap.nonce,
        admin_fee,
    )?;

    Ok(())
}

/// Processes an [Withdraw](enum.Instruction.html).
fn process_withdraw(
    program_id: &Pubkey,
    pool_token_amount: u64,
    minimum_token_a_amount: u64,
    minimum_token_b_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if pool_token_amount == 0 {
        // noop
        return Ok(());
    }
    let account_info_iter = &mut accounts.iter();
    let swap_info = next_account_info(account_info_iter)?;
    let swap_authority_info = next_account_info(account_info_iter)?;
    let user_authority_info = next_account_info(account_info_iter)?;
    let pool_mint_info = next_account_info(account_info_iter)?;
    let source_info = next_account_info(account_info_iter)?;
    let token_a_info = next_account_info(account_info_iter)?;
    let token_b_info = next_account_info(account_info_iter)?;
    let dest_token_a_info = next_account_info(account_info_iter)?;
    let dest_token_b_info = next_account_info(account_info_iter)?;
    let admin_fee_dest_a_info = next_account_info(account_info_iter)?;
    let admin_fee_dest_b_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let clock_sysvar_info = next_account_info(account_info_iter)?;

    let token_swap = SwapInfo::unpack(&swap_info.data.borrow())?;
    check_swap_authority(
        &token_swap,
        swap_info.key,
        program_id,
        swap_authority_info.key,
    )?;

    check_withdraw_token_accounts(
        &token_swap.token_a,
        token_a_info.key,
        admin_fee_dest_a_info.key,
    )?;

    check_withdraw_token_accounts(
        &token_swap.token_b,
        token_b_info.key,
        admin_fee_dest_b_info.key,
    )?;

    check_keys_equal!(
        *pool_mint_info.key,
        token_swap.pool_mint,
        "Pool mint",
        SwapError::IncorrectMint
    );

    let pool_mint = utils::unpack_mint(&pool_mint_info.data.borrow())?;
    if pool_mint.supply == 0 {
        return Err(SwapError::EmptyPool.into());
    }

    let token_a = utils::unpack_token_account(&token_a_info.data.borrow())?;
    let token_b = utils::unpack_token_account(&token_b_info.data.borrow())?;

    let converter = PoolTokenConverter {
        supply: (pool_mint.supply),
        token_a: (token_a.amount),
        token_b: (token_b.amount),
        fees: &token_swap.fees,
    };
    let pool_token_amount_u256 = pool_token_amount;

    let ctx = WithdrawContext {
        token_swap,
        token_program_info,
        swap_authority_info,
        swap_info,
    };

    let (a_amount, a_fee, a_admin_fee) = check_can_withdraw_token(
        converter.token_a_rate(pool_token_amount_u256),
        minimum_token_a_amount,
    )?;
    let (b_amount, b_fee, b_admin_fee) = check_can_withdraw_token(
        converter.token_b_rate(pool_token_amount_u256),
        minimum_token_b_amount,
    )?;

    handle_token_withdraw(
        &ctx,
        (a_amount, a_admin_fee),
        token_a_info,
        dest_token_a_info,
        admin_fee_dest_a_info,
    )?;
    handle_token_withdraw(
        &ctx,
        (b_amount, b_admin_fee),
        token_b_info,
        dest_token_b_info,
        admin_fee_dest_b_info,
    )?;

    // burn LP tokens withdrawn
    token::burn(
        token_program_info.clone(),
        source_info.clone(),
        pool_mint_info.clone(),
        user_authority_info.clone(),
        pool_token_amount,
    )?;

    let clock = Clock::from_account_info(clock_sysvar_info)?;
    log_event(
        Event::WithdrawA,
        clock.unix_timestamp,
        a_amount,
        0,
        0,
        a_fee,
    );
    log_event(
        Event::WithdrawB,
        clock.unix_timestamp,
        0,
        b_amount,
        0,
        b_fee,
    );
    log_event(
        Event::Burn,
        clock.unix_timestamp,
        0,
        0,
        pool_token_amount,
        0,
    );

    Ok(())
}

/// Processes an [WithdrawOne](enum.Instruction.html).
fn process_withdraw_one(
    program_id: &Pubkey,
    pool_token_amount: u64,
    minimum_token_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if pool_token_amount == 0 {
        // noop
        return Ok(());
    }

    let account_info_iter = &mut accounts.iter();
    let swap_info = next_account_info(account_info_iter)?;
    let swap_authority_info = next_account_info(account_info_iter)?;
    let user_authority_info = next_account_info(account_info_iter)?;
    let pool_mint_info = next_account_info(account_info_iter)?;
    let source_info = next_account_info(account_info_iter)?;
    let base_token_info = next_account_info(account_info_iter)?;
    let quote_token_info = next_account_info(account_info_iter)?;
    let destination_info = next_account_info(account_info_iter)?;
    let admin_destination_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let clock_sysvar_info = next_account_info(account_info_iter)?;

    if *base_token_info.key == *quote_token_info.key {
        return Err(SwapError::InvalidInput.into());
    }

    let token_swap = SwapInfo::unpack(&swap_info.data.borrow())?;
    if token_swap.is_paused {
        return Err(SwapError::IsPaused.into());
    }
    check_swap_authority(
        &token_swap,
        swap_info.key,
        program_id,
        swap_authority_info.key,
    )?;

    if *base_token_info.key == token_swap.token_a.reserves {
        check_keys_equal!(
            *quote_token_info.key,
            token_swap.token_b.reserves,
            "Swap A -> B reserves",
            SwapError::IncorrectSwapAccount
        );
        check_keys_equal!(
            *admin_destination_info.key,
            token_swap.token_a.admin_fees,
            "Swap A -> B admin fee destination",
            SwapError::InvalidAdmin
        );
    } else if *base_token_info.key == token_swap.token_b.reserves {
        check_keys_equal!(
            *quote_token_info.key,
            token_swap.token_a.reserves,
            "Swap B -> A reserves",
            SwapError::IncorrectSwapAccount
        );
        check_keys_equal!(
            *admin_destination_info.key,
            token_swap.token_b.admin_fees,
            "Swap B -> A admin fee destination",
            SwapError::InvalidAdmin
        );
    } else {
        msg!("Unknown base token:");
        base_token_info.key.log();
        return Err(SwapError::IncorrectSwapAccount.into());
    }

    check_keys_equal!(
        *pool_mint_info.key,
        token_swap.pool_mint,
        "Pool mint",
        SwapError::IncorrectMint
    );

    let pool_mint = utils::unpack_mint(&pool_mint_info.data.borrow())?;
    let clock = Clock::from_account_info(clock_sysvar_info)?;
    let base_token = utils::unpack_token_account(&base_token_info.data.borrow())?;
    let quote_token = utils::unpack_token_account(&quote_token_info.data.borrow())?;

    let invariant = StableSwap::new(
        token_swap.initial_amp_factor,
        token_swap.target_amp_factor,
        clock.unix_timestamp,
        token_swap.start_ramp_ts,
        token_swap.stop_ramp_ts,
    );
    let (dy, dy_fee) = invariant
        .compute_withdraw_one(
            pool_token_amount,
            pool_mint.supply,
            base_token.amount,
            quote_token.amount,
            &token_swap.fees,
        )
        .ok_or(SwapError::CalculationFailure)?;
    let withdraw_fee = token_swap
        .fees
        .withdraw_fee(dy)
        .ok_or(SwapError::CalculationFailure)?;
    let token_amount = dy
        .checked_sub(withdraw_fee)
        .ok_or(SwapError::CalculationFailure)?;
    if token_amount < minimum_token_amount {
        log_slippage_error(minimum_token_amount, token_amount);
        return Err(SwapError::ExceededSlippage.into());
    }

    let admin_trade_fee = token_swap
        .fees
        .admin_trade_fee(dy_fee)
        .ok_or(SwapError::CalculationFailure)?;
    let admin_withdraw_fee = token_swap
        .fees
        .admin_withdraw_fee(withdraw_fee)
        .ok_or(SwapError::CalculationFailure)?;
    let admin_fee = admin_trade_fee
        .checked_add(admin_withdraw_fee)
        .ok_or(SwapError::CalculationFailure)?;

    // from swap to user
    token::transfer_as_swap(
        swap_info.key,
        token_program_info.clone(),
        base_token_info.clone(),
        destination_info.clone(),
        swap_authority_info.clone(),
        token_swap.nonce,
        token_amount,
    )?;
    // from swap to fee
    token::transfer_as_swap(
        swap_info.key,
        token_program_info.clone(),
        base_token_info.clone(),
        admin_destination_info.clone(),
        swap_authority_info.clone(),
        token_swap.nonce,
        admin_fee,
    )?;
    token::burn(
        token_program_info.clone(),
        source_info.clone(),
        pool_mint_info.clone(),
        user_authority_info.clone(),
        pool_token_amount,
    )?;

    if *base_token_info.key == token_swap.token_a.reserves {
        log_event(
            Event::WithdrawA,
            clock.unix_timestamp,
            token_amount,
            0,
            0,
            dy_fee,
        );
    } else {
        log_event(
            Event::WithdrawB,
            clock.unix_timestamp,
            0,
            token_amount,
            0,
            dy_fee,
        );
    };
    log_event(
        Event::Burn,
        clock.unix_timestamp,
        0,
        0,
        pool_token_amount,
        0,
    );

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        instruction::{deposit, swap, withdraw, withdraw_one},
        processor::test_utils::*,
    };
    use solana_program::program_error::ProgramError;
    use solana_sdk::account::Account;
    use spl_token::{
        error::TokenError,
        instruction::{set_authority, AuthorityType},
    };

    /// Initial amount of pool tokens for swap contract, hard-coded to something
    /// "sensible" given a maximum of u64.
    /// Note that on Ethereum, Uniswap uses the geometric mean of all provided
    /// input amounts, and Balancer uses 100 * 10 ^ 18.
    const INITIAL_SWAP_POOL_AMOUNT: u64 = 1_000_000_000;

    #[test]
    fn test_initialize() {
        let user_key = pubkey_rand();
        let amp_factor = MIN_AMP;
        let token_a_amount = 1000;
        let token_b_amount = 2000;
        let pool_token_amount = 10;
        let mut accounts = SwapAccountInfo::new(
            &user_key,
            amp_factor,
            token_a_amount,
            token_b_amount,
            DEFAULT_TEST_FEES,
        );
        // wrong nonce for authority_key
        {
            let old_nonce = accounts.nonce;
            accounts.nonce = old_nonce - 1;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.initialize_swap()
            );
            accounts.nonce = old_nonce;
        }

        // invalid amp factors
        {
            let old_initial_amp_factor = accounts.initial_amp_factor;
            accounts.initial_amp_factor = MIN_AMP - 1;
            // amp factor too low
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.initialize_swap()
            );
            accounts.initial_amp_factor = MAX_AMP + 1;
            // amp factor too high
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.initialize_swap()
            );
            accounts.initial_amp_factor = old_initial_amp_factor;
        }

        // uninitialized token a account
        {
            let old_account = accounts.token_a_account;
            accounts.token_a_account = Account::default();
            assert_eq!(
                Err(SwapError::ExpectedAccount.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // uninitialized token b account
        {
            let old_account = accounts.token_b_account;
            accounts.token_b_account = Account::default();
            assert_eq!(
                Err(SwapError::ExpectedAccount.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // uninitialized pool mint
        {
            let old_account = accounts.pool_mint_account;
            accounts.pool_mint_account = Account::default();
            assert_eq!(
                Err(SwapError::ExpectedMint.into()),
                accounts.initialize_swap()
            );
            accounts.pool_mint_account = old_account;
        }

        // token A account owner is not swap authority
        {
            let (_token_a_key, token_a_account) = mint_token(
                &spl_token::id(),
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &user_key,
                0,
            );
            let old_account = accounts.token_a_account;
            accounts.token_a_account = token_a_account;
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // token B account owner is not swap authority
        {
            let (_token_b_key, token_b_account) = mint_token(
                &spl_token::id(),
                &accounts.token_b_mint_key,
                &mut accounts.token_b_mint_account,
                &user_key,
                &user_key,
                0,
            );
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_b_account;
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // pool token account owner is swap authority
        {
            let (_pool_token_key, pool_token_account) = mint_token(
                &spl_token::id(),
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.pool_token_account;
            accounts.pool_token_account = pool_token_account;
            assert_eq!(
                Err(SwapError::InvalidOutputOwner.into()),
                accounts.initialize_swap()
            );
            accounts.pool_token_account = old_account;
        }

        // pool mint authority is not swap authority
        {
            let (_pool_mint_key, pool_mint_account) =
                create_mint(&spl_token::id(), &user_key, DEFAULT_TOKEN_DECIMALS, None);
            let old_mint = accounts.pool_mint_account;
            accounts.pool_mint_account = pool_mint_account;
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                accounts.initialize_swap()
            );
            accounts.pool_mint_account = old_mint;
        }

        // pool mint token has freeze authority
        {
            let (_pool_mint_key, pool_mint_account) = create_mint(
                &spl_token::id(),
                &accounts.authority_key,
                DEFAULT_TOKEN_DECIMALS,
                Some(&user_key),
            );
            let old_mint = accounts.pool_mint_account;
            accounts.pool_mint_account = pool_mint_account;
            assert_eq!(
                Err(SwapError::InvalidFreezeAuthority.into()),
                accounts.initialize_swap()
            );
            accounts.pool_mint_account = old_mint;
        }

        // empty token A account
        {
            let (_token_a_key, token_a_account) = mint_token(
                &spl_token::id(),
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.token_a_account;
            accounts.token_a_account = token_a_account;
            assert_eq!(
                Err(SwapError::EmptySupply.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // empty token B account
        {
            let (_token_b_key, token_b_account) = mint_token(
                &spl_token::id(),
                &accounts.token_b_mint_key,
                &mut accounts.token_b_mint_account,
                &user_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_b_account;
            assert_eq!(
                Err(SwapError::EmptySupply.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // invalid pool tokens
        {
            let old_mint = accounts.pool_mint_account;
            let old_pool_account = accounts.pool_token_account;

            let (_pool_mint_key, pool_mint_account) = create_mint(
                &spl_token::id(),
                &accounts.authority_key,
                DEFAULT_TOKEN_DECIMALS,
                None,
            );
            accounts.pool_mint_account = pool_mint_account;

            let (_empty_pool_token_key, empty_pool_token_account) = mint_token(
                &spl_token::id(),
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &user_key,
                0,
            );

            let (_pool_token_key, pool_token_account) = mint_token(
                &spl_token::id(),
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &user_key,
                pool_token_amount,
            );

            // non-empty pool token account
            accounts.pool_token_account = pool_token_account;
            assert_eq!(
                Err(SwapError::InvalidSupply.into()),
                accounts.initialize_swap()
            );

            // pool tokens already in circulation
            accounts.pool_token_account = empty_pool_token_account;
            assert_eq!(
                Err(SwapError::InvalidSupply.into()),
                accounts.initialize_swap()
            );

            accounts.pool_mint_account = old_mint;
            accounts.pool_token_account = old_pool_account;
        }

        // token A account has close authority
        {
            do_process_instruction(
                set_authority(
                    &spl_token::id(),
                    &accounts.token_a_key,
                    Some(&user_key),
                    AuthorityType::CloseAccount,
                    &accounts.authority_key,
                    &[],
                )
                .unwrap(),
                vec![&mut accounts.token_a_account, &mut Account::default()],
            )
            .unwrap();
            assert_eq!(
                Err(SwapError::InvalidCloseAuthority.into()),
                accounts.initialize_swap()
            );

            do_process_instruction(
                set_authority(
                    &spl_token::id(),
                    &accounts.token_a_key,
                    None,
                    AuthorityType::CloseAccount,
                    &user_key,
                    &[],
                )
                .unwrap(),
                vec![&mut accounts.token_a_account, &mut Account::default()],
            )
            .unwrap();
        }

        // token B account has close authority
        {
            do_process_instruction(
                set_authority(
                    &spl_token::id(),
                    &accounts.token_b_key,
                    Some(&user_key),
                    AuthorityType::CloseAccount,
                    &accounts.authority_key,
                    &[],
                )
                .unwrap(),
                vec![&mut accounts.token_b_account, &mut Account::default()],
            )
            .unwrap();
            assert_eq!(
                Err(SwapError::InvalidCloseAuthority.into()),
                accounts.initialize_swap()
            );

            do_process_instruction(
                set_authority(
                    &spl_token::id(),
                    &accounts.token_b_key,
                    None,
                    AuthorityType::CloseAccount,
                    &user_key,
                    &[],
                )
                .unwrap(),
                vec![&mut accounts.token_b_account, &mut Account::default()],
            )
            .unwrap();
        }

        // mismatched admin mints
        {
            let (wrong_admin_fee_key, wrong_admin_fee_account) = mint_token(
                &spl_token::id(),
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &user_key,
                0,
            );

            // wrong admin_fee_key_a
            let old_admin_fee_account_a = accounts.admin_fee_a_account;
            let old_admin_fee_key_a = accounts.admin_fee_a_key;
            accounts.admin_fee_a_account = wrong_admin_fee_account.clone();
            accounts.admin_fee_a_key = wrong_admin_fee_key;

            assert_eq!(
                Err(SwapError::InvalidAdmin.into()),
                accounts.initialize_swap()
            );

            accounts.admin_fee_a_account = old_admin_fee_account_a;
            accounts.admin_fee_a_key = old_admin_fee_key_a;

            // wrong admin_fee_key_b
            let old_admin_fee_account_b = accounts.admin_fee_b_account;
            let old_admin_fee_key_b = accounts.admin_fee_b_key;
            accounts.admin_fee_b_account = wrong_admin_fee_account;
            accounts.admin_fee_b_key = wrong_admin_fee_key;

            assert_eq!(
                Err(SwapError::InvalidAdmin.into()),
                accounts.initialize_swap()
            );

            accounts.admin_fee_b_account = old_admin_fee_account_b;
            accounts.admin_fee_b_key = old_admin_fee_key_b;
        }

        // mismatched mint decimals
        {
            let (bad_mint_key, mut bad_mint_account) =
                create_mint(&spl_token::id(), &accounts.authority_key, 2, None);

            // Pool mint decimal does not match
            let old_pool_mint_key = accounts.pool_mint_key;
            let old_pool_mint_account = accounts.pool_mint_account;
            accounts.pool_mint_key = bad_mint_key;
            accounts.pool_mint_account = bad_mint_account.clone();

            assert_eq!(
                Err(SwapError::MismatchedDecimals.into()),
                accounts.initialize_swap()
            );

            accounts.pool_mint_key = old_pool_mint_key;
            accounts.pool_mint_account = old_pool_mint_account;

            // Token a mint decimal does not match token b decimals
            let (bad_token_key, bad_token_account) = mint_token(
                &spl_token::id(),
                &bad_mint_key,
                &mut bad_mint_account,
                &accounts.authority_key,
                &accounts.authority_key,
                10,
            );

            let old_token_a_key = accounts.token_a_key;
            let old_token_a_account = accounts.token_a_account;
            let old_token_a_mint_key = accounts.token_a_mint_key;
            let old_token_a_mint_account = accounts.token_a_mint_account;
            accounts.token_a_key = bad_token_key;
            accounts.token_a_account = bad_token_account;
            accounts.token_a_mint_key = bad_mint_key;
            accounts.token_a_mint_account = bad_mint_account;

            assert_eq!(
                Err(SwapError::MismatchedDecimals.into()),
                accounts.initialize_swap()
            );

            accounts.token_a_key = old_token_a_key;
            accounts.token_a_account = old_token_a_account;
            accounts.token_a_mint_key = old_token_a_mint_key;
            accounts.token_a_mint_account = old_token_a_mint_account;
        }

        // create swap with same token A and B
        {
            let (_token_a_repeat_key, token_a_repeat_account) = mint_token(
                &spl_token::id(),
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &accounts.authority_key,
                10,
            );
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_a_repeat_account;
            assert_eq!(
                Err(SwapError::RepeatedMint.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // create valid swap
        accounts.initialize_swap().unwrap();

        // create again
        {
            assert_eq!(
                Err(SwapError::AlreadyInUse.into()),
                accounts.initialize_swap()
            );
        }
        let swap_info = SwapInfo::unpack(&accounts.swap_account.data).unwrap();
        assert_eq!(swap_info.is_initialized, true);
        assert_eq!(swap_info.is_paused, false);
        assert_eq!(swap_info.nonce, accounts.nonce);
        assert_eq!(swap_info.initial_amp_factor, amp_factor);
        assert_eq!(swap_info.target_amp_factor, amp_factor);
        assert_eq!(swap_info.start_ramp_ts, ZERO_TS);
        assert_eq!(swap_info.stop_ramp_ts, ZERO_TS);
        assert_eq!(swap_info.future_admin_deadline, ZERO_TS);
        assert_eq!(swap_info.future_admin_key, Pubkey::default());
        assert_eq!(swap_info.admin_key, accounts.admin_key);
        assert_eq!(swap_info.token_a.reserves, accounts.token_a_key);
        assert_eq!(swap_info.token_b.reserves, accounts.token_b_key);
        assert_eq!(swap_info.pool_mint, accounts.pool_mint_key);
        assert_eq!(swap_info.token_a.mint, accounts.token_a_mint_key);
        assert_eq!(swap_info.token_b.mint, accounts.token_b_mint_key);
        assert_eq!(swap_info.token_a.admin_fees, accounts.admin_fee_a_key);
        assert_eq!(swap_info.token_b.admin_fees, accounts.admin_fee_b_key);
        assert_eq!(swap_info.fees, DEFAULT_TEST_FEES);
        let token_a = utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
        assert_eq!(token_a.amount, token_a_amount);
        let token_b = utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
        assert_eq!(token_b.amount, token_b_amount);
        let pool_account = utils::unpack_token_account(&accounts.pool_token_account.data).unwrap();
        let pool_mint = utils::unpack_mint(&accounts.pool_mint_account.data).unwrap();
        assert_eq!(pool_mint.supply, pool_account.amount);
    }

    #[test]
    fn test_deposit() {
        let user_key = pubkey_rand();
        let depositor_key = pubkey_rand();
        let amp_factor = MIN_AMP;
        let token_a_amount = 1000;
        let token_b_amount = 9000;
        let mut accounts = SwapAccountInfo::new(
            &user_key,
            amp_factor,
            token_a_amount,
            token_b_amount,
            DEFAULT_TEST_FEES,
        );

        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;
        let min_mint_amount = 0;

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong nonce for authority_key
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &spl_token::id(),
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // not enough token A
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &depositor_key,
                deposit_a / 2,
                deposit_b,
                0,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
        }

        // not enough token B
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &depositor_key,
                deposit_a,
                deposit_b / 2,
                0,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
        }

        // swap account as source account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                do_process_instruction(
                    deposit(
                        &SWAP_PROGRAM_ID,
                        &spl_token::id(),
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &depositor_key,
                        &accounts.token_a_key,
                        &token_b_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        deposit_a,
                        deposit_b,
                        min_mint_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.token_a_account.clone(),
                        &mut token_b_account,
                        &mut accounts.token_a_account.clone(),
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                )
            );
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                do_process_instruction(
                    deposit(
                        &SWAP_PROGRAM_ID,
                        &spl_token::id(),
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &depositor_key,
                        &token_a_key,
                        &accounts.token_b_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        deposit_a,
                        deposit_b,
                        min_mint_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_b_account.clone(),
                        &mut accounts.token_a_account.clone(),
                        &mut accounts.token_b_account.clone(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_b_key,
                    &mut token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                mut _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let (
                wrong_token_key,
                mut wrong_token_account,
                _token_b_key,
                mut _token_b_account,
                _pool_key,
                mut _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &wrong_token_key,
                    &mut wrong_token_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let wrong_key = pubkey_rand();
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                do_process_instruction(
                    deposit(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &depositor_key,
                        &token_a_key,
                        &token_b_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        deposit_a,
                        deposit_b,
                        min_mint_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut token_b_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);

            let old_a_key = accounts.token_a_key;
            let old_a_account = accounts.token_a_account;

            accounts.token_a_key = token_a_key;
            accounts.token_a_account = token_a_account.clone();

            // wrong swap token a account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );

            accounts.token_a_key = old_a_key;
            accounts.token_a_account = old_a_account;

            let old_b_key = accounts.token_b_key;
            let old_b_account = accounts.token_b_account;

            accounts.token_b_key = token_b_key;
            accounts.token_b_account = token_b_account.clone();

            // wrong swap token b account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );

            accounts.token_b_key = old_b_key;
            accounts.token_b_account = old_b_account;
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let (pool_mint_key, pool_mint_account) = create_mint(
                &spl_token::id(),
                &accounts.authority_key,
                DEFAULT_TOKEN_DECIMALS,
                None,
            );
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectMint.into()),
                accounts.deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            // min mint_amount in too high
            let high_min_mint_amount = 10000000000000;
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.deposit(
                    &depositor_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    deposit_a,
                    deposit_b,
                    high_min_mint_amount,
                )
            );
        }

        // correctly deposit
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            accounts
                .deposit(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
                .unwrap();

            let swap_token_a = utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
            assert_eq!(swap_token_a.amount, deposit_a + token_a_amount);
            let swap_token_b = utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
            assert_eq!(swap_token_b.amount, deposit_b + token_b_amount);
            let token_a = utils::unpack_token_account(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, 0);
            let token_b = utils::unpack_token_account(&token_b_account.data).unwrap();
            assert_eq!(token_b.amount, 0);
            let pool_account = utils::unpack_token_account(&pool_account.data).unwrap();
            let swap_pool_account =
                utils::unpack_token_account(&accounts.pool_token_account.data).unwrap();
            let pool_mint = utils::unpack_mint(&accounts.pool_mint_account.data).unwrap();
            // XXX: Revisit and make sure amount of LP tokens minted is corrected.
            assert_eq!(
                pool_mint.supply,
                pool_account.amount + swap_pool_account.amount
            );
        }

        // Pool is paused
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            // Pause pool
            accounts.pause().unwrap();

            assert_eq!(
                Err(SwapError::IsPaused.into()),
                accounts.deposit(
                    &depositor_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    deposit_a,
                    deposit_b,
                    min_mint_amount,
                )
            );
        }
    }

    #[test]
    fn test_withdraw() {
        let user_key = pubkey_rand();
        let amp_factor = MIN_AMP;
        let token_a_amount = 1000;
        let token_b_amount = 2000;
        let mut accounts = SwapAccountInfo::new(
            &user_key,
            amp_factor,
            token_a_amount,
            token_b_amount,
            DEFAULT_TEST_FEES,
        );
        let withdrawer_key = pubkey_rand();
        let initial_a = token_a_amount / 10;
        let initial_b = token_b_amount / 10;
        let initial_pool = INITIAL_SWAP_POOL_AMOUNT;
        let withdraw_amount = initial_pool / 4;
        let minimum_a_amount = initial_a / 40;
        let minimum_b_amount = initial_b / 40;

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong nonce for authority_key
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &spl_token::id(),
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // not enough pool tokens
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount / 2,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount / 2,
                    minimum_b_amount / 2,
                )
            );
        }

        // wrong token a / b accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );
        }

        // wrong admin a / b accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );
            let (
                wrong_admin_a_key,
                wrong_admin_a_account,
                wrong_admin_b_key,
                wrong_admin_b_account,
                _pool_key,
                mut _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );

            let old_admin_a_key = accounts.admin_fee_a_key;
            let old_admin_a_account = accounts.admin_fee_a_account;
            accounts.admin_fee_a_key = wrong_admin_a_key;
            accounts.admin_fee_a_account = wrong_admin_a_account;

            assert_eq!(
                Err(SwapError::InvalidAdmin.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );

            accounts.admin_fee_a_key = old_admin_a_key;
            accounts.admin_fee_a_account = old_admin_a_account;

            let old_admin_b_key = accounts.admin_fee_b_key;
            let old_admin_b_account = accounts.admin_fee_b_account;
            accounts.admin_fee_b_key = wrong_admin_b_key;
            accounts.admin_fee_b_account = wrong_admin_b_account;

            assert_eq!(
                Err(SwapError::InvalidAdmin.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );

            accounts.admin_fee_b_key = old_admin_b_key;
            accounts.admin_fee_b_account = old_admin_b_account;
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );
            let (
                wrong_pool_key,
                mut wrong_pool_account,
                _token_b_key,
                _token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                withdraw_amount,
                initial_b,
                withdraw_amount,
            );
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &wrong_pool_key,
                    &mut wrong_pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );
            let wrong_key = pubkey_rand();
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                do_process_instruction(
                    withdraw(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &withdrawer_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_a_key,
                        &token_b_key,
                        &accounts.admin_fee_a_key,
                        &accounts.admin_fee_b_key,
                        withdraw_amount,
                        minimum_a_amount,
                        minimum_b_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_a_account,
                        &mut token_b_account,
                        &mut accounts.admin_fee_a_account,
                        &mut accounts.admin_fee_b_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );

            let old_a_key = accounts.token_a_key;
            let old_a_account = accounts.token_a_account;

            accounts.token_a_key = token_a_key;
            accounts.token_a_account = token_a_account.clone();

            // wrong swap token a account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );

            accounts.token_a_key = old_a_key;
            accounts.token_a_account = old_a_account;

            let old_b_key = accounts.token_b_key;
            let old_b_account = accounts.token_b_account;

            accounts.token_b_key = token_b_key;
            accounts.token_b_account = token_b_account.clone();

            // wrong swap token b account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );

            accounts.token_b_key = old_b_key;
            accounts.token_b_account = old_b_account;
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );
            let (pool_mint_key, pool_mint_account) = create_mint(
                &spl_token::id(),
                &accounts.authority_key,
                DEFAULT_TOKEN_DECIMALS,
                None,
            );
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectMint.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );
            // minimum A amount out too high
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount * 30, // XXX: 10 -> 30: Revisit this slippage multiplier
                    minimum_b_amount,
                )
            );
            // minimum B amount out too high
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount * 30, // XXX: 10 -> 30; Revisit this slippage multiplier
                )
            );
        }

        // correct withdrawal
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );

            let original_reserve_a = utils::unpack_token_account(&accounts.token_a_account.data)
                .unwrap()
                .amount;
            let original_reserve_b = utils::unpack_token_account(&accounts.token_b_account.data)
                .unwrap()
                .amount;
            let original_lp_supply = utils::unpack_mint(&accounts.pool_mint_account.data)
                .unwrap()
                .supply;

            accounts
                .withdraw(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_a_amount,
                    minimum_b_amount,
                )
                .unwrap();

            let swap_token_a = utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
            let swap_token_b = utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
            let pool_converter = PoolTokenConverter {
                supply: original_lp_supply.into(),
                token_a: original_reserve_a.into(),
                token_b: original_reserve_b.into(),
                fees: &DEFAULT_TEST_FEES,
            };

            let (withdrawn_a, _, admin_fee_a) =
                pool_converter.token_a_rate(withdraw_amount).unwrap();
            let withdrawn_total_a = withdrawn_a + admin_fee_a;
            assert_eq!(swap_token_a.amount, token_a_amount - withdrawn_total_a);
            let (withdrawn_b, _, admin_fee_b) =
                pool_converter.token_b_rate(withdraw_amount).unwrap();
            let withdrawn_total_b = withdrawn_b + admin_fee_b;
            assert_eq!(swap_token_b.amount, token_b_amount - withdrawn_total_b);
            let token_a = utils::unpack_token_account(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, initial_a + (withdrawn_a));
            let token_b = utils::unpack_token_account(&token_b_account.data).unwrap();
            assert_eq!(token_b.amount, initial_b + (withdrawn_b));
            let pool_account = utils::unpack_token_account(&pool_account.data).unwrap();
            assert_eq!(pool_account.amount, initial_pool - withdraw_amount);
            let admin_fee_key_a =
                utils::unpack_token_account(&accounts.admin_fee_a_account.data).unwrap();
            assert_eq!(admin_fee_key_a.amount, (admin_fee_a));
            let admin_fee_key_b =
                utils::unpack_token_account(&accounts.admin_fee_b_account.data).unwrap();
            assert_eq!(admin_fee_key_b.amount, (admin_fee_b));
        }
    }

    #[test]
    fn test_swap() {
        let user_key = pubkey_rand();
        let swapper_key = pubkey_rand();
        let amp_factor = 85;
        let token_a_amount = 5000;
        let token_b_amount = 5000;
        let mut accounts = SwapAccountInfo::new(
            &user_key,
            amp_factor,
            token_a_amount,
            token_b_amount,
            DEFAULT_TEST_FEES,
        );
        let initial_a = token_a_amount / 5;
        let initial_b = token_b_amount / 5;
        let minimum_b_amount = initial_b / 2;

        let swap_token_a_key = accounts.token_a_key;
        let swap_token_b_key = accounts.token_b_key;

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_b_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong nonce
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &spl_token::id(),
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_b_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let wrong_program_id = pubkey_rand();
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                do_process_instruction(
                    swap(
                        &SWAP_PROGRAM_ID,
                        &wrong_program_id,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_key,
                        &token_a_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_b_key,
                        &accounts.admin_fee_b_key,
                        initial_a,
                        minimum_b_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_b_account,
                        &mut accounts.admin_fee_b_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                ),
            );
        }

        // not enough token a to swap
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a * 2,
                    minimum_b_amount * 2,
                )
            );
        }

        // wrong swap token A / B accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                do_process_instruction(
                    swap(
                        &SWAP_PROGRAM_ID,
                        &spl_token::id(),
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &swapper_key,
                        &token_a_key,
                        &token_a_key,
                        &token_b_key,
                        &token_b_key,
                        &accounts.admin_fee_b_key,
                        initial_a,
                        minimum_b_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account.clone(),
                        &mut token_a_account,
                        &mut token_b_account.clone(),
                        &mut token_b_account,
                        &mut accounts.admin_fee_b_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                ),
            );
        }

        // wrong admin account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                wrong_admin_key,
                mut wrong_admin_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::InvalidAdmin.into()),
                do_process_instruction(
                    swap(
                        &SWAP_PROGRAM_ID,
                        &spl_token::id(),
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &swapper_key,
                        &token_a_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_b_key,
                        &wrong_admin_key,
                        initial_a,
                        minimum_b_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_b_account,
                        &mut wrong_admin_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                ),
            );
        }

        // wrong user token A / B accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.swap(
                    &swapper_key,
                    &token_b_key,
                    &mut token_b_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_a_key,
                    &mut token_a_account,
                    initial_a,
                    minimum_b_amount,
                )
            );
        }

        // swap from a to a
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account.clone(),
                    &swap_token_a_key,
                    &swap_token_a_key,
                    &token_a_key,
                    &mut token_a_account,
                    initial_a,
                    minimum_b_amount,
                )
            );
        }

        // swap source same as swap account
        {
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.swap(
                    &accounts.authority_key.clone(),
                    &swap_token_a_key,
                    &mut accounts.token_a_account.clone(),
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &swap_token_b_key,
                    &mut accounts.token_b_account.clone(),
                    initial_a,
                    minimum_b_amount,
                )
            );
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.swap(
                    &accounts.authority_key.clone(),
                    &swap_token_b_key,
                    &mut accounts.token_b_account.clone(),
                    &swap_token_b_key,
                    &swap_token_a_key,
                    &swap_token_a_key,
                    &mut accounts.token_a_account.clone(),
                    initial_b,
                    initial_a / 2,
                )
            );
        }

        // slippage exceeded: minimum out amount too high
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_b_amount * 2,
                )
            );
        }

        // correct swap
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            // swap one way
            let a_to_b_amount = initial_a / 10;
            let minimum_b_amount = initial_b / 20;
            accounts
                .swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    a_to_b_amount,
                    minimum_b_amount,
                )
                .unwrap();

            let invariant = StableSwap::new(
                accounts.initial_amp_factor,
                accounts.target_amp_factor,
                ZERO_TS,
                ZERO_TS,
                ZERO_TS,
            );
            let result = invariant
                .swap_to(
                    a_to_b_amount,
                    token_a_amount,
                    token_b_amount,
                    &DEFAULT_TEST_FEES,
                )
                .unwrap();

            let swap_token_a = utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
            let token_a_amount = swap_token_a.amount;
            assert_eq!(token_a_amount, 5100);
            assert_eq!(token_a_amount, (result.new_source_amount));
            let token_a = utils::unpack_token_account(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, initial_a - a_to_b_amount);

            let swap_token_b = utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
            let token_b_amount = swap_token_b.amount;
            assert_eq!(token_b_amount, 4903);
            assert_eq!(token_b_amount, (result.new_destination_amount));
            let token_b = utils::unpack_token_account(&token_b_account.data).unwrap();
            assert_eq!(token_b.amount, 1094);
            assert_eq!(token_b.amount, initial_b + (result.amount_swapped));
            let admin_fee_b_account =
                utils::unpack_token_account(&accounts.admin_fee_b_account.data).unwrap();
            assert_eq!(admin_fee_b_account.amount, (result.admin_fee));

            let first_swap_amount = result.amount_swapped;

            // swap the other way
            let b_to_a_amount = initial_b / 10;
            let minimum_a_amount = initial_a / 20;
            accounts
                .swap(
                    &swapper_key,
                    &token_b_key,
                    &mut token_b_account,
                    &swap_token_b_key,
                    &swap_token_a_key,
                    &token_a_key,
                    &mut token_a_account,
                    b_to_a_amount,
                    minimum_a_amount,
                )
                .unwrap();

            let invariant = StableSwap::new(
                accounts.initial_amp_factor,
                accounts.target_amp_factor,
                ZERO_TS,
                ZERO_TS,
                ZERO_TS,
            );
            let result = invariant
                .swap_to(
                    b_to_a_amount,
                    token_b_amount,
                    token_a_amount,
                    &DEFAULT_TEST_FEES,
                )
                .unwrap();

            let swap_token_a = utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
            assert_eq!(swap_token_a.amount, 5002);
            assert_eq!(swap_token_a.amount, (result.new_destination_amount));
            let token_a = utils::unpack_token_account(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, 995);
            assert_eq!(
                token_a.amount,
                initial_a - a_to_b_amount + (result.amount_swapped)
            );

            let swap_token_b = utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
            assert_eq!(swap_token_b.amount, 5003);
            assert_eq!(swap_token_b.amount, (result.new_source_amount));
            let token_b = utils::unpack_token_account(&token_b_account.data).unwrap();
            assert_eq!(token_b.amount, 994);
            assert_eq!(
                token_b.amount,
                initial_b + (first_swap_amount) - b_to_a_amount
            );
            let admin_fee_a_account =
                utils::unpack_token_account(&accounts.admin_fee_a_account.data).unwrap();
            assert_eq!(admin_fee_a_account.amount, (result.admin_fee));
        }

        // Pool is paused
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            // Pause pool
            accounts.pause().unwrap();

            assert_eq!(
                Err(SwapError::IsPaused.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_b_amount,
                )
            );
        }
    }

    #[test]
    fn test_withdraw_one() {
        let user_key = pubkey_rand();
        let amp_factor = MIN_AMP;
        let token_a_amount = 1000;
        let token_b_amount = 1000;
        let mut accounts = SwapAccountInfo::new(
            &user_key,
            amp_factor,
            token_a_amount,
            token_b_amount,
            DEFAULT_TEST_FEES,
        );
        let withdrawer_key = pubkey_rand();
        let initial_a = token_a_amount / 10;
        let initial_b = token_b_amount / 10;
        let initial_pool = initial_a + initial_b;
        // Withdraw entire pool share
        let withdraw_amount = initial_pool;
        let minimum_amount = 0;

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong nonce for authority_key
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &spl_token::id(),
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // same swap / quote accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );

            let old_token_b_key = accounts.token_b_key;
            let old_token_b_account = accounts.token_b_account;
            accounts.token_b_key = accounts.token_a_key;
            accounts.token_b_account = accounts.token_a_account.clone();

            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );

            accounts.token_b_key = old_token_b_key;
            accounts.token_b_account = old_token_b_account;
        }

        // foreign swap / quote accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );
            let foreign_authority = pubkey_rand();
            let (foreign_mint_key, mut foreign_mint_account) = create_mint(
                &spl_token::id(),
                &foreign_authority,
                DEFAULT_TOKEN_DECIMALS,
                None,
            );
            let (foreign_token_key, foreign_token_account) = mint_token(
                &spl_token::id(),
                &foreign_mint_key,
                &mut foreign_mint_account,
                &foreign_authority,
                &pubkey_rand(),
                0,
            );

            let old_token_a_key = accounts.token_a_key;
            let old_token_a_account = accounts.token_a_account;
            accounts.token_a_key = foreign_token_key;
            accounts.token_a_account = foreign_token_account.clone();

            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );

            accounts.token_a_key = old_token_a_key;
            accounts.token_a_account = old_token_a_account;

            let old_token_b_key = accounts.token_b_key;
            let old_token_b_account = accounts.token_b_account;
            accounts.token_b_key = foreign_token_key;
            accounts.token_b_account = foreign_token_account;

            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );

            accounts.token_b_key = old_token_b_key;
            accounts.token_b_account = old_token_b_account;
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                wrong_token_b_key,
                mut wrong_token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                withdraw_amount,
                withdraw_amount,
            );
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &wrong_token_b_key,
                    &mut wrong_token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );
            let wrong_key = pubkey_rand();
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                do_process_instruction(
                    withdraw_one(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &withdrawer_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_a_key,
                        &accounts.admin_fee_a_key,
                        withdraw_amount,
                        minimum_amount,
                    )
                    .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_a_account,
                        &mut accounts.admin_fee_a_account,
                        &mut Account::default(),
                        &mut clock_account(ZERO_TS),
                    ],
                )
            );
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );
            let (pool_mint_key, pool_mint_account) = create_mint(
                &spl_token::id(),
                &accounts.authority_key,
                DEFAULT_TOKEN_DECIMALS,
                None,
            );
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectMint.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // wrong destination account
        {
            let (
                _token_a_key,
                _token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );

            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );
        }

        // wrong admin account
        {
            let (
                wrong_admin_key,
                wrong_admin_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount,
            );

            let old_admin_a_key = accounts.admin_fee_a_key;
            let old_admin_a_account = accounts.admin_fee_a_account;
            accounts.admin_fee_a_key = wrong_admin_key;
            accounts.admin_fee_a_account = wrong_admin_account;

            assert_eq!(
                Err(SwapError::InvalidAdmin.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );

            accounts.admin_fee_a_key = old_admin_a_key;
            accounts.admin_fee_a_account = old_admin_a_account;
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );

            let high_minimum_amount = 100000;
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    high_minimum_amount,
                )
            );
        }

        // correct withdraw
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );

            let old_swap_token_a =
                utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
            let old_swap_token_b =
                utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
            let old_pool_mint = utils::unpack_mint(&accounts.pool_mint_account.data).unwrap();

            let invariant = StableSwap::new(
                accounts.initial_amp_factor,
                accounts.target_amp_factor,
                ZERO_TS,
                ZERO_TS,
                ZERO_TS,
            );
            let (withdraw_one_amount_before_fees, withdraw_one_trade_fee) = invariant
                .compute_withdraw_one(
                    withdraw_amount.into(),
                    old_pool_mint.supply.into(),
                    old_swap_token_a.amount.into(),
                    old_swap_token_b.amount.into(),
                    &DEFAULT_TEST_FEES,
                )
                .unwrap();
            let withdraw_one_withdraw_fee = DEFAULT_TEST_FEES
                .withdraw_fee(withdraw_one_amount_before_fees)
                .unwrap();
            let expected_withdraw_one_amount =
                withdraw_one_amount_before_fees - withdraw_one_withdraw_fee;
            let expected_admin_fee = DEFAULT_TEST_FEES
                .admin_trade_fee(withdraw_one_trade_fee)
                .unwrap()
                + DEFAULT_TEST_FEES
                    .admin_withdraw_fee(withdraw_one_withdraw_fee)
                    .unwrap();

            accounts
                .withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
                .unwrap();

            let swap_token_a = utils::unpack_token_account(&accounts.token_a_account.data).unwrap();
            assert_eq!(
                old_swap_token_a.amount - swap_token_a.amount - expected_admin_fee,
                (expected_withdraw_one_amount)
            );
            let admin_fee_key_a =
                utils::unpack_token_account(&accounts.admin_fee_a_account.data).unwrap();
            assert_eq!(admin_fee_key_a.amount, expected_admin_fee);
            let swap_token_b = utils::unpack_token_account(&accounts.token_b_account.data).unwrap();
            assert_eq!(swap_token_b.amount, old_swap_token_b.amount);
            let pool_mint = utils::unpack_mint(&accounts.pool_mint_account.data).unwrap();
            assert_eq!(pool_mint.supply, old_pool_mint.supply - withdraw_amount);
        }

        // pool is paused
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool,
            );
            // pause pool
            accounts.pause().unwrap();

            assert_eq!(
                Err(SwapError::IsPaused.into()),
                accounts.withdraw_one(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount,
                    minimum_amount,
                )
            );
        }
    }
}
