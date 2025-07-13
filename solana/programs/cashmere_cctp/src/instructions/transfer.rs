use anchor_lang::{prelude::*, solana_program::{
    self,
    sysvar::instructions as sysvar,
}, system_program};
use anchor_spl::token::{
    self,
    Token,
    TokenAccount,
    Transfer as SplTransfer
};
use borsh::{BorshSerialize, to_vec};
use crate::{
    utils::{
        verify_ed25519_ix,
        calculate_fee,
    },
    state::{
        Custodian,
        Config,
    },
    events::TransferEvent,
    cctp::{
        TOKEN_MESSENGER_MINTER_PROGRAM_ID,
        MESSAGE_TRANSMITTER_PROGRAM_ID,
        token_messenger_minter_program::{
            LocalToken,
            cpi::{
                DepositForBurn,
                DepositForBurnParams,
            },
        },
    },
    errors::TransferError,
};

#[derive(BorshSerialize)]
struct TransferParams {
    local_domain: u32,
    destination_domain: u32,
    fee: u64,
    deadline: u64,
    fee_is_native: bool,
}

/*
fee structure:
- percentage fee is always taken in USDC
- server-side fee is taken either in USDC or SOL, depending on `fee_is_native`
- gas drop is taken either in USDC or SOL, depending on `fee_is_native`
*/

pub fn transfer_ix(
    ctx: Context<TransferContext>,
    usdc_amount: u64,
    destination_domain: u32,
    recipient: [u8; 32],
    solana_owner: [u8; 32],
    fee: u64,
    deadline: u64,
    gas_drop_amount: u64,
    fee_is_native: bool,
) -> Result<()> {
    if ctx.accounts.token_program.key() != token::ID {
        return Err(TransferError::InvalidTokenProgram.into());
    }

    let msg = TransferParams {
        local_domain: 5,
        destination_domain,
        fee,
        deadline,
        fee_is_native,
    };
    let msg_bytes = to_vec(&msg)?;
    let ed25519_ix = &ctx.accounts.signature.to_account_info();
    match verify_ed25519_ix(ed25519_ix, &msg_bytes, &ctx.accounts.config.signer_key) {
        Err(e) => return Err(e),
        _ => {},
    };

    let clock = Clock::get()?;
    if clock.unix_timestamp as u64 > deadline {
        return Err(TransferError::DeadlineExpired.into());
    }

    let usdc_fee_amount = calculate_fee(usdc_amount, ctx.accounts.config.fee_bp, if fee_is_native { 0 } else { fee });
    require!(usdc_amount >= usdc_fee_amount, TransferError::FeeExceedsAmount);
    if fee_is_native {
        let native_gas_drop_limit = ctx.accounts.config.max_native_gas_drop[destination_domain as usize];
        require!(native_gas_drop_limit == 0 || gas_drop_amount <= native_gas_drop_limit, TransferError::GasDropLimitExceeded);
    } else {
        let usdc_gas_drop_limit = ctx.accounts.config.max_usdc_gas_drop;
        require!(usdc_gas_drop_limit == 0 || gas_drop_amount <= usdc_gas_drop_limit, TransferError::GasDropLimitExceeded);
    }

    // collect fee in USDC
    token::transfer(CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        SplTransfer {
            from: ctx.accounts.owner_token_account.to_account_info(),
            to: ctx.accounts.fee_collector_usdc_account.to_account_info(),
            authority: ctx.accounts.owner.to_account_info(),
        },
    ), usdc_fee_amount)?;
    
    if fee_is_native {
        // collect fee in SOL
        system_program::transfer(CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.owner.to_account_info(),
                to: ctx.accounts.fee_collector_sol_account.to_account_info(),
            },
        ), fee)?;
        // collect gas drop in SOL
        if gas_drop_amount > 0 {
            system_program::transfer(CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.owner.to_account_info(),
                    to: ctx.accounts.gas_drop_collector_sol_account.to_account_info(),
                },
            ), gas_drop_amount)?;
        }
    } else {
        // collect gas drop in USDC
        if gas_drop_amount > 0 {
            token::transfer(CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                SplTransfer {
                    from: ctx.accounts.owner_token_account.to_account_info(),
                    to: ctx.accounts.gas_drop_collector_usdc_account.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ), gas_drop_amount)?;
        }
    }

    let amount = usdc_amount - usdc_fee_amount;

    let custodian_seeds: &[&[&[u8]]] = &[&[Custodian::SEED_PREFIX, &[ctx.accounts.custodian.bump]]];

    // transfer the rest
    token::transfer(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        SplTransfer {
            from: ctx.accounts.owner_token_account.to_account_info(),
            to: ctx.accounts.burn_token_account.to_account_info(),
            authority: ctx.accounts.custodian.to_account_info(),
        },
        custodian_seeds,
    ), amount)?;

    let cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_messenger_minter_program.to_account_info(),
        DepositForBurn {
            owner: ctx.accounts.custodian.to_account_info(),
            event_rent_payer: ctx.accounts.owner.to_account_info(),
            sender_authority_pda: ctx.accounts.token_messenger_minter_sender_authority.to_account_info(),
            burn_token_account: ctx.accounts.burn_token_account.to_account_info(),
            message_transmitter: ctx.accounts.message_transmitter.to_account_info(),
            token_messenger: ctx.accounts.token_messenger.to_account_info(),
            remote_token_messenger: ctx.accounts.remote_token_messenger.to_account_info(),
            token_minter: ctx.accounts.token_minter.to_account_info(),
            local_token: ctx.accounts.local_token.to_account_info(),
            burn_token_mint: ctx.accounts.burn_token_mint.to_account_info(),
            message_sent_event_data: ctx.accounts.message_sent_event_data.to_account_info(),
            message_transmitter_program: ctx.accounts.message_transmitter_program.to_account_info(),
            token_messenger_minter_program: ctx.accounts.token_messenger_minter_program.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
            event_authority: ctx.accounts.token_messenger_minter_event_authority.to_account_info(),
        },
        custodian_seeds,
    );

    let args = DepositForBurnParams {
        amount,
        destination_domain,
        mint_recipient: recipient,
    };

    const ANCHOR_IX_SELECTOR: [u8; 8] = [215, 60, 61, 46, 114, 55, 128, 176];

    solana_program::program::invoke_signed(
        &solana_program::instruction::Instruction {
            program_id: TOKEN_MESSENGER_MINTER_PROGRAM_ID,
            accounts: cpi_ctx.to_account_metas(None),
            data: (ANCHOR_IX_SELECTOR, args).try_to_vec()?,
        },
        &cpi_ctx.to_account_infos(),
        cpi_ctx.signer_seeds,
    )?;

    ctx.accounts.config.nonce += 1;

    emit!(TransferEvent {
        destination_domain,
        nonce: ctx.accounts.config.nonce,
        recipient,
        solana_owner,
        user: ctx.accounts.owner.key(),
        amount,
        gas_drop_amount,
        cctp_nonce: -1,
        fee_is_native,
        cctp_message: ctx.accounts.message_sent_event_data.key(),
    });

    token::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        token::CloseAccount {
            account: ctx.accounts.burn_token_account.to_account_info(),
            destination: ctx.accounts.owner.to_account_info(),
            authority: ctx.accounts.custodian.to_account_info(),
        },
        custodian_seeds,
    ))
}

#[derive(Accounts)]
pub struct TransferContext<'info> {
    #[account(mut, seeds=[b"config"], bump)]
    pub config: Box<Account<'info, Config>>,

    #[account(mut)]
    pub owner_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        address = config.fee_collector_sol,
    )]
    pub fee_collector_sol_account: SystemAccount<'info>,
    #[account(
        mut,
        address = config.fee_collector_usdc,
    )]
    pub fee_collector_usdc_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        address = config.gas_drop_collector_sol,
    )]
    pub gas_drop_collector_sol_account: SystemAccount<'info>,
    #[account(
        mut,
        address = config.gas_drop_collector_usdc,
    )]
    pub gas_drop_collector_usdc_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub owner: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,


    // cctp

    /// This program's emitter authority.
    ///
    /// Seeds must be \["emitter"\].
    #[account(
        seeds = [Custodian::SEED_PREFIX],
        bump = custodian.bump,
    )]
    custodian: Account<'info, Custodian>,

    /// Circle-supported mint.
    ///
    /// CHECK: Mutable. This token account's mint must be the same as the one found in the CCTP
    /// Token Messenger Minter program's local token account.
    #[account(
        mut,
        address = local_token.mint,
    )]
    burn_token_mint: AccountInfo<'info>,

    /// Token account where assets are burned from. The CCTP Token Messenger Minter program will
    /// burn the configured [amount](TransferTokensWithPayloadArgs::amount) from this account.
    ///
    /// NOTE: Transfer authority must be delegated to the custodian because this instruction
    /// transfers assets from this account to the custody token account.
    #[account(
        mut,
        token::mint = burn_token_mint
    )]
    burn_source: Account<'info, token::TokenAccount>,

    /// Temporary custody token account. This account will be closed at the end of this instruction.
    /// It just acts as a conduit to allow this program to be the transfer initiator in the CCTP
    /// message.
    ///
    /// Seeds must be \["__custody"\].
    #[account(
        init,
        payer = owner,
        token::mint = burn_token_mint,
        token::authority = custodian,
        seeds = [b"__custody"],
        bump,
    )]
    burn_token_account: Account<'info, token::TokenAccount>,

    /// Local token account, which this program uses to validate the `mint` used to burn.
    ///
    /// Mutable. Seeds must be \["local_token", mint\] (CCTP Token Messenger Minter program).
    #[account(mut)]
    local_token: Box<Account<'info, LocalToken>>,

    /// CHECK: Must equal CCTP Token Messenger Minter program ID.
    #[account(address = TOKEN_MESSENGER_MINTER_PROGRAM_ID)]
    token_messenger_minter_program: UncheckedAccount<'info>,

    /// CHECK: Must equal CCTP Message Transmitter program ID.
    #[account(address = MESSAGE_TRANSMITTER_PROGRAM_ID)]
    message_transmitter_program: UncheckedAccount<'info>,

    /// CHECK: Seeds must be \["__event_authority"\] (CCTP Token Messenger Minter program).
    token_messenger_minter_event_authority: UncheckedAccount<'info>,

    /// CHECK: Mutable. Seeds must be \["message_transmitter"\] (CCTP Message Transmitter program).
    #[account(mut)]
    message_transmitter: UncheckedAccount<'info>,

    /// CHECK: Seeds must be \["token_messenger"\] (CCTP Token Messenger Minter program).
    token_messenger: UncheckedAccount<'info>,

    /// CHECK: Seeds must be \["remote_token_messenger"\, remote_domain.to_string()] (CCTP Token
    /// Messenger Minter program).
    remote_token_messenger: UncheckedAccount<'info>,

    /// CHECK: Seeds must be \["token_minter"\] (CCTP Token Messenger Minter program).
    token_minter: UncheckedAccount<'info>,

    /// CHECK: Mutable signer to create CCTP message.
    #[account(mut)]
    message_sent_event_data: Signer<'info>,

    /// CHECK: Seeds must be \["sender_authority"\] (CCTP Token Messenger Minter program).
    token_messenger_minter_sender_authority: UncheckedAccount<'info>,

    /// CHECK: Safe because it's a sysvar account
    #[account(address = sysvar::ID)]
    pub signature: AccountInfo<'info>,
}
