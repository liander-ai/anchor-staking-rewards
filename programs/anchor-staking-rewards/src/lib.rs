use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, Transfer},
};

declare_id!("BGnGLvL5nBwwPwJtbaqCCRzihjU5uETzv1vo6WTCgS2c");

// A token staking vault.
//
// Users stake an SPL token into a program-owned vault and accrue rewards over
// time, proportional to the amount staked and the elapsed seconds. Rewards are
// paid from a separate reward vault funded by the admin.
//
//   reward = staked_amount * elapsed_seconds * reward_rate / ACC_PRECISION
//
// The pool tracks total staked; each user has their own stake account recording
// their balance and the timestamp rewards were last settled.

const ACC_PRECISION: u128 = 1_000_000_000_000; // 1e12, fixed-point scaling

#[program]
pub mod anchor_staking_rewards {
    use super::*;

    /// Create the pool. `reward_rate` is reward tokens per staked token per
    /// second, scaled by ACC_PRECISION.
    pub fn initialize_pool(ctx: Context<InitializePool>, reward_rate: u64) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.admin = ctx.accounts.admin.key();
        pool.stake_mint = ctx.accounts.stake_mint.key();
        pool.reward_mint = ctx.accounts.reward_mint.key();
        pool.stake_vault = ctx.accounts.stake_vault.key();
        pool.reward_vault = ctx.accounts.reward_vault.key();
        pool.reward_rate = reward_rate;
        pool.total_staked = 0;
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// Admin tops up the reward vault so the pool can pay out claims.
    pub fn fund_rewards(ctx: Context<FundRewards>, amount: u64) -> Result<()> {
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.admin_reward_account.to_account_info(),
                    to: ctx.accounts.reward_vault.to_account_info(),
                    authority: ctx.accounts.admin.to_account_info(),
                },
            ),
            amount,
        )
    }

    /// Stake `amount` tokens. Settles any pending rewards first so the new
    /// balance does not retroactively change past accrual.
    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::ZeroAmount);

        let now = Clock::get()?.unix_timestamp;
        let stake_account = &mut ctx.accounts.stake_account;

        if stake_account.owner == Pubkey::default() {
            stake_account.owner = ctx.accounts.user.key();
            stake_account.pool = ctx.accounts.pool.key();
            stake_account.amount = 0;
            stake_account.reward_debt = 0;
            stake_account.last_update = now;
            stake_account.bump = ctx.bumps.stake_account;
        }

        settle_rewards(stake_account, ctx.accounts.pool.reward_rate, now)?;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.user_stake_account.to_account_info(),
                    to: ctx.accounts.stake_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        stake_account.amount = stake_account
            .amount
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;
        let pool = &mut ctx.accounts.pool;
        pool.total_staked = pool
            .total_staked
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;
        Ok(())
    }

    /// Unstake `amount` tokens, settling rewards first.
    pub fn unstake(ctx: Context<Unstake>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::ZeroAmount);
        let stake_account = &mut ctx.accounts.stake_account;
        require!(stake_account.amount >= amount, StakingError::InsufficientStake);

        let now = Clock::get()?.unix_timestamp;
        settle_rewards(stake_account, ctx.accounts.pool.reward_rate, now)?;

        stake_account.amount -= amount;
        let pool = &mut ctx.accounts.pool;
        pool.total_staked = pool
            .total_staked
            .checked_sub(amount)
            .ok_or(StakingError::MathOverflow)?;

        let seeds: &[&[u8]] = &[b"pool", pool.stake_mint.as_ref(), &[pool.bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.stake_vault.to_account_info(),
                    to: ctx.accounts.user_stake_account.to_account_info(),
                    authority: pool.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )
    }

    /// Claim all accrued rewards.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let stake_account = &mut ctx.accounts.stake_account;
        settle_rewards(stake_account, ctx.accounts.pool.reward_rate, now)?;

        let reward = stake_account.reward_debt;
        require!(reward > 0, StakingError::NothingToClaim);
        stake_account.reward_debt = 0;

        let pool = &ctx.accounts.pool;
        let seeds: &[&[u8]] = &[b"pool", pool.stake_mint.as_ref(), &[pool.bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.reward_vault.to_account_info(),
                    to: ctx.accounts.user_reward_account.to_account_info(),
                    authority: pool.to_account_info(),
                },
                &[seeds],
            ),
            reward,
        )
    }
}

/// Accrue rewards from `last_update` to `now` into `reward_debt`.
fn settle_rewards(stake_account: &mut StakeAccount, reward_rate: u64, now: i64) -> Result<()> {
    let elapsed = now
        .checked_sub(stake_account.last_update)
        .ok_or(StakingError::MathOverflow)?;
    if elapsed > 0 && stake_account.amount > 0 {
        let accrued = (stake_account.amount as u128)
            .checked_mul(elapsed as u128)
            .and_then(|v| v.checked_mul(reward_rate as u128))
            .map(|v| v / ACC_PRECISION)
            .ok_or(StakingError::MathOverflow)?;
        stake_account.reward_debt = stake_account
            .reward_debt
            .checked_add(accrued as u64)
            .ok_or(StakingError::MathOverflow)?;
    }
    stake_account.last_update = now;
    Ok(())
}

#[derive(Accounts)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + Pool::INIT_SPACE,
        seeds = [b"pool", stake_mint.key().as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    pub stake_mint: Account<'info, Mint>,
    pub reward_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = stake_mint,
        associated_token::authority = pool
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = reward_mint,
        associated_token::authority = pool
    )]
    pub reward_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundRewards<'info> {
    #[account(mut, address = pool.admin)]
    pub admin: Signer<'info>,

    #[account(seeds = [b"pool", pool.stake_mint.as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(mut, address = pool.reward_vault)]
    pub reward_vault: Account<'info, TokenAccount>,

    #[account(mut, token::authority = admin, token::mint = pool.reward_mint)]
    pub admin_reward_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [b"pool", pool.stake_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + StakeAccount::INIT_SPACE,
        seeds = [b"stake", pool.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub stake_account: Account<'info, StakeAccount>,

    #[account(mut, address = pool.stake_vault)]
    pub stake_vault: Account<'info, TokenAccount>,

    #[account(mut, token::authority = user, token::mint = pool.stake_mint)]
    pub user_stake_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [b"pool", pool.stake_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"stake", pool.key().as_ref(), user.key().as_ref()],
        bump = stake_account.bump,
        has_one = owner
    )]
    pub stake_account: Account<'info, StakeAccount>,

    /// CHECK: constrained via has_one = owner on stake_account
    #[account(address = user.key())]
    pub owner: UncheckedAccount<'info>,

    #[account(mut, address = pool.stake_vault)]
    pub stake_vault: Account<'info, TokenAccount>,

    #[account(mut, token::authority = user, token::mint = pool.stake_mint)]
    pub user_stake_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    pub user: Signer<'info>,

    #[account(
        seeds = [b"pool", pool.stake_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"stake", pool.key().as_ref(), user.key().as_ref()],
        bump = stake_account.bump,
        has_one = owner
    )]
    pub stake_account: Account<'info, StakeAccount>,

    /// CHECK: constrained via has_one = owner on stake_account
    #[account(address = user.key())]
    pub owner: UncheckedAccount<'info>,

    #[account(mut, address = pool.reward_vault)]
    pub reward_vault: Account<'info, TokenAccount>,

    #[account(mut, token::authority = user, token::mint = pool.reward_mint)]
    pub user_reward_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub admin: Pubkey,
    pub stake_mint: Pubkey,
    pub reward_mint: Pubkey,
    pub stake_vault: Pubkey,
    pub reward_vault: Pubkey,
    pub reward_rate: u64,
    pub total_staked: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct StakeAccount {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub amount: u64,
    pub reward_debt: u64,
    pub last_update: i64,
    pub bump: u8,
}

#[error_code]
pub enum StakingError {
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Insufficient staked balance")]
    InsufficientStake,
    #[msg("No rewards to claim")]
    NothingToClaim,
    #[msg("Arithmetic overflow")]
    MathOverflow,
}
