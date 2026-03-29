#![allow(deprecated)]
#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("ED4rS9ZKmoMwmgqDtK1Vw7fLLcdqcJtBymSGfGqLVpvR");

#[program]
pub mod token_staking {
    use super::*;

    /// Initialize the global staking pool (admin only)
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        reward_rate: u64, // tokens rewarded per second per staked token (scaled by 1e9)
        lock_period: i64, // seconds users must wait before unstaking (0 = no lock)
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.admin = ctx.accounts.admin.key();
        pool.stake_mint = ctx.accounts.stake_mint.key();
        pool.reward_mint = ctx.accounts.reward_mint.key();
        pool.reward_vault = ctx.accounts.reward_vault.key();
        pool.reward_rate = reward_rate;
        pool.lock_period = lock_period;
        pool.total_staked = 0;
        pool.is_paused = false;
        pool.bump = ctx.bumps.pool;
        emit!(PoolInitialized {
            admin: pool.admin,
            stake_mint: pool.stake_mint,
            reward_rate,
            lock_period,
        });
        Ok(())
    }

    /// Fund the reward vault so rewards can be paid out
    pub fn fund_rewards(ctx: Context<FundRewards>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.pool.is_paused, StakingError::PoolPaused);
        require!(amount > 0, StakingError::ZeroAmount);
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.funder_reward_account.to_account_info(),
                    to: ctx.accounts.reward_vault.to_account_info(),
                    authority: ctx.accounts.funder.to_account_info(),
                },
            ),
            amount,
        )?;
        emit!(RewardsFunded {
            funder: ctx.accounts.funder.key(),
            amount,
        });
        Ok(())
    }

    /// Stake tokens into the pool
    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.pool.is_paused, StakingError::PoolPaused);
        require!(amount > 0, StakingError::ZeroAmount);

        let clock = Clock::get()?;
        let pool = &mut ctx.accounts.pool;
        let user_stake = &mut ctx.accounts.user_stake;

        // Settle any existing pending rewards before modifying stake
        if user_stake.amount > 0 {
            let pending = calculate_pending_rewards(
                user_stake.amount,
                user_stake.last_update_ts,
                clock.unix_timestamp,
                pool.reward_rate,
            );
            user_stake.rewards_earned = user_stake
                .rewards_earned
                .checked_add(pending)
                .ok_or(StakingError::MathOverflow)?;
        }

        // Transfer stake tokens from user -> pool vault
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_token_account.to_account_info(),
                    to: ctx.accounts.stake_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        // Update user stake record
        user_stake.owner = ctx.accounts.user.key();
        user_stake.pool = pool.key();
        user_stake.amount = user_stake
            .amount
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;
        user_stake.stake_ts = if user_stake.stake_ts == 0 {
            clock.unix_timestamp
        } else {
            user_stake.stake_ts
        };
        user_stake.last_update_ts = clock.unix_timestamp;
        user_stake.unlock_ts = clock.unix_timestamp + pool.lock_period;
        user_stake.bump = ctx.bumps.user_stake;

        // Update pool total
        pool.total_staked = pool
            .total_staked
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;

        emit!(Staked {
            user: ctx.accounts.user.key(),
            amount,
            total_staked: user_stake.amount,
        });
        Ok(())
    }

    /// Claim accumulated reward tokens
    pub fn claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
        require!(!ctx.accounts.pool.is_paused, StakingError::PoolPaused);

        let clock = Clock::get()?;
        let pool = &ctx.accounts.pool;
        let user_stake = &mut ctx.accounts.user_stake;

        // Calculate new rewards since last update
        let pending = calculate_pending_rewards(
            user_stake.amount,
            user_stake.last_update_ts,
            clock.unix_timestamp,
            pool.reward_rate,
        );
        let total_claimable = user_stake
            .rewards_earned
            .checked_add(pending)
            .ok_or(StakingError::MathOverflow)?;

        require!(total_claimable > 0, StakingError::NoRewards);
        require!(
            ctx.accounts.reward_vault.amount >= total_claimable,
            StakingError::InsufficientRewards
        );

        // Reset reward tracking
        user_stake.rewards_earned = 0;
        user_stake.last_update_ts = clock.unix_timestamp;

        // Transfer rewards from vault -> user (pool is signer via PDA)
        let seeds = &[
            b"pool".as_ref(),
            ctx.accounts.pool.stake_mint.as_ref(),
            &[ctx.accounts.pool.bump],
        ];

        let signer_seeds = &[&seeds[..]];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.reward_vault.to_account_info(),
                    to: ctx.accounts.user_reward_account.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                signer_seeds,
            ),
            total_claimable,
        )?;

        emit!(RewardsClaimed {
            user: ctx.accounts.user.key(),
            amount: total_claimable,
        });
        Ok(())
    }

    /// Unstake tokens (respects lock period)
    pub fn unstake(ctx: Context<Unstake>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.pool.is_paused, StakingError::PoolPaused);
        require!(amount > 0, StakingError::ZeroAmount);

        let clock = Clock::get()?;
        let pool = &mut ctx.accounts.pool;
        let user_stake = &mut ctx.accounts.user_stake;

        require!(user_stake.amount >= amount, StakingError::InsufficientStake);

        // Enforce lock period
        if pool.lock_period > 0 {
            require!(
                clock.unix_timestamp >= user_stake.unlock_ts,
                StakingError::StillLocked
            );
        }

        // Settle pending rewards before reducing stake
        let pending = calculate_pending_rewards(
            user_stake.amount,
            user_stake.last_update_ts,
            clock.unix_timestamp,
            pool.reward_rate,
        );
        user_stake.rewards_earned = user_stake
            .rewards_earned
            .checked_add(pending)
            .ok_or(StakingError::MathOverflow)?;
        user_stake.last_update_ts = clock.unix_timestamp;

        // Reduce user stake
        user_stake.amount = user_stake
            .amount
            .checked_sub(amount)
            .ok_or(StakingError::MathOverflow)?;
        pool.total_staked = pool
            .total_staked
            .checked_sub(amount)
            .ok_or(StakingError::MathOverflow)?;

        // Transfer tokens from stake vault -> user (pool PDA signs)
        let seeds = &[
            b"pool".as_ref(),
            ctx.accounts.pool.stake_mint.as_ref(),
            &[ctx.accounts.pool.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.stake_vault.to_account_info(),
                    to: ctx.accounts.user_token_account.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
        )?;

        emit!(Unstaked {
            user: ctx.accounts.user.key(),
            amount,
            remaining: user_stake.amount,
        });
        Ok(())
    }

    //  Admin Instructions

    /// Pause or unpause the pool
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.pool.is_paused = paused;
        emit!(PauseToggled { paused });
        Ok(())
    }

    /// Update the reward rate (tokens per second per staked token, scaled 1e9)
    pub fn set_reward_rate(ctx: Context<AdminOnly>, new_rate: u64) -> Result<()> {
        ctx.accounts.pool.reward_rate = new_rate;
        emit!(RewardRateUpdated { new_rate });
        Ok(())
    }

    /// Update the lock period in seconds
    pub fn set_lock_period(ctx: Context<AdminOnly>, new_lock_period: i64) -> Result<()> {
        require!(new_lock_period >= 0, StakingError::InvalidLockPeriod);
        ctx.accounts.pool.lock_period = new_lock_period;
        emit!(LockPeriodUpdated { new_lock_period });
        Ok(())
    }

    /// Transfer admin authority to a new address
    pub fn transfer_admin(ctx: Context<TransferAdmin>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.pool.admin = new_admin;
        emit!(AdminTransferred { new_admin });
        Ok(())
    }
}

//  Helper Functions

/// Calculate rewards earned between two timestamps
/// Formula: amount * rate * elapsed_seconds / 1_000_000_000
fn calculate_pending_rewards(
    staked_amount: u64,
    last_update_ts: i64,
    current_ts: i64,
    reward_rate: u64,
) -> u64 {
    if last_update_ts == 0 || current_ts <= last_update_ts {
        return 0;
    }
    let elapsed = (current_ts - last_update_ts) as u64;
    // Use u128 to prevent overflow in intermediate calculation
    let rewards = (staked_amount as u128)
        .saturating_mul(reward_rate as u128)
        .saturating_mul(elapsed as u128)
        / 1_000_000_000u128;
    rewards.min(u64::MAX as u128) as u64
}

//  Account Structs

#[account]
#[derive(Default)]
pub struct Pool {
    pub admin: Pubkey,        // 32
    pub stake_mint: Pubkey,   // 32
    pub reward_mint: Pubkey,  // 32
    pub reward_vault: Pubkey, // 32
    pub reward_rate: u64,     // 8  — tokens/sec/staked_token × 1e9
    pub lock_period: i64,     // 8  — seconds
    pub total_staked: u64,    // 8
    pub is_paused: bool,      // 1
    pub bump: u8,             // 1
}

impl Pool {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 8 + 8 + 8 + 1 + 1 + 64; // 64 bytes padding
}

#[account]
#[derive(Default)]
pub struct UserStake {
    pub owner: Pubkey,       // 32
    pub pool: Pubkey,        // 32
    pub amount: u64,         // 8
    pub stake_ts: i64,       // 8  — first stake timestamp
    pub last_update_ts: i64, // 8  — last reward settlement
    pub unlock_ts: i64,      // 8  — when tokens can be unstaked
    pub rewards_earned: u64, // 8  — accrued but unclaimed rewards
    pub bump: u8,            // 1
}

impl UserStake {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 1 + 64;
}

//  Contexts

#[derive(Accounts)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    pub stake_mint: Account<'info, Mint>,
    pub reward_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        space = Pool::LEN,
        seeds = [b"pool", stake_mint.key().as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    /// Pool-owned vault that holds reward tokens. Authority = pool PDA.
    #[account(
        init,
        payer = admin,
        token::mint = reward_mint,
        token::authority = pool,
    )]
    pub reward_vault: Account<'info, TokenAccount>,

    /// Pool-owned vault that holds staked tokens. Authority = pool PDA.
    #[account(
        init,
        payer = admin,
        token::mint = stake_mint,
        token::authority = pool,
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct FundRewards<'info> {
    pub funder: Signer<'info>,

    #[account(mut)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        constraint = funder_reward_account.mint == pool.reward_mint,
        constraint = funder_reward_account.owner == funder.key(),
    )]
    pub funder_reward_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = reward_vault.key() == pool.reward_vault,
    )]
    pub reward_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub pool: Account<'info, Pool>,

    #[account(
        init_if_needed,
        payer = user,
        space = UserStake::LEN,
        seeds = [b"user_stake", pool.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub user_stake: Account<'info, UserStake>,

    #[account(
        mut,
        constraint = user_token_account.mint == pool.stake_mint,
        constraint = user_token_account.owner == user.key(),
    )]
    pub user_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = stake_vault.mint == pool.stake_mint,
        constraint = stake_vault.owner == pool.key(),
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    pub user: Signer<'info>,

    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"user_stake", pool.key().as_ref(), user.key().as_ref()],
        bump = user_stake.bump,
        constraint = user_stake.owner == user.key(),
    )]
    pub user_stake: Account<'info, UserStake>,

    #[account(
        mut,
        constraint = reward_vault.key() == pool.reward_vault,
    )]
    pub reward_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_reward_account.mint == pool.reward_mint,
        constraint = user_reward_account.owner == user.key(),
    )]
    pub user_reward_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    pub user: Signer<'info>,

    #[account(mut)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"user_stake", pool.key().as_ref(), user.key().as_ref()],
        bump = user_stake.bump,
        constraint = user_stake.owner == user.key(),
    )]
    pub user_stake: Account<'info, UserStake>,

    #[account(
        mut,
        constraint = user_token_account.mint == pool.stake_mint,
        constraint = user_token_account.owner == user.key(),
    )]
    pub user_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = stake_vault.mint == pool.stake_mint,
        constraint = stake_vault.owner == pool.key(),
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(constraint = admin.key() == pool.admin @ StakingError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(mut)]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct TransferAdmin<'info> {
    #[account(constraint = admin.key() == pool.admin @ StakingError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(mut)]
    pub pool: Account<'info, Pool>,
}

//  Errors

#[error_code]
pub enum StakingError {
    #[msg("Pool is currently paused")]
    PoolPaused,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("No rewards to claim")]
    NoRewards,
    #[msg("Insufficient rewards in vault")]
    InsufficientRewards,
    #[msg("Insufficient staked amount")]
    InsufficientStake,
    #[msg("Tokens are still within the lock period")]
    StillLocked,
    #[msg("Unauthorized: caller is not admin")]
    Unauthorized,
    #[msg("Lock period cannot be negative")]
    InvalidLockPeriod,
}

//  Events

#[event]
pub struct PoolInitialized {
    pub admin: Pubkey,
    pub stake_mint: Pubkey,
    pub reward_rate: u64,
    pub lock_period: i64,
}

#[event]
pub struct RewardsFunded {
    pub funder: Pubkey,
    pub amount: u64,
}

#[event]
pub struct Staked {
    pub user: Pubkey,
    pub amount: u64,
    pub total_staked: u64,
}

#[event]
pub struct RewardsClaimed {
    pub user: Pubkey,
    pub amount: u64,
}

#[event]
pub struct Unstaked {
    pub user: Pubkey,
    pub amount: u64,
    pub remaining: u64,
}

#[event]
pub struct PauseToggled {
    pub paused: bool,
}

#[event]
pub struct RewardRateUpdated {
    pub new_rate: u64,
}

#[event]
pub struct LockPeriodUpdated {
    pub new_lock_period: i64,
}

#[event]
pub struct AdminTransferred {
    pub new_admin: Pubkey,
}
