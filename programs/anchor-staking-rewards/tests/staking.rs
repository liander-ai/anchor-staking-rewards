//! End-to-end tests for the staking-rewards program.
//!
//! These run the *actual compiled program* (`anchor_staking_rewards.so`) inside
//! LiteSVM and drive it exactly as a client would: real SPL mints and token
//! accounts, real PDAs, and a manipulated clock to exercise the time-based
//! reward accrual. Instruction *data* is produced by Anchor's generated
//! `instruction` structs (just bytes); account metas are assembled by hand so
//! the test stays on a single `solana-*` 3.x type universe (Anchor links an
//! older `solana-pubkey`, which would otherwise clash with LiteSVM's).

use anchor_lang::InstructionData;
use anchor_staking_rewards::instruction as ix_data;
use litesvm::LiteSVM;
use litesvm_token::{
    get_spl_account, spl_token::state::Account as SplTokenAccount,
    CreateAssociatedTokenAccount, CreateMint, MintTo,
};
use solana_clock::Clock;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const PROGRAM_SO: &[u8] = include_bytes!("../../../target/deploy/anchor_staking_rewards.so");

const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("BGnGLvL5nBwwPwJtbaqCCRzihjU5uETzv1vo6WTCgS2c");
const TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSTEM_PROGRAM: Pubkey =
    Pubkey::from_str_const("11111111111111111111111111111111");

const DECIMALS: u8 = 6;
/// reward tokens per staked token per second, scaled by 1e12 in-program.
/// With this rate, `reward = staked * elapsed / 1_000_000`.
const REWARD_RATE: u64 = 1_000_000;
const SOL: u64 = 1_000_000_000;

/// Classic associated-token-account derivation.
fn ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), TOKEN_PROGRAM.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

/// Submit a single instruction, returning `Err(log_string)` if it fails.
fn send(
    svm: &mut LiteSVM,
    ix: Instruction,
    payer: &Keypair,
    signers: &[&Keypair],
) -> Result<(), String> {
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers)
        .map_err(|e| format!("sign error: {e:?}"))?;
    svm.send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e.err))
}

fn token_balance(svm: &LiteSVM, account: &Pubkey) -> u64 {
    get_spl_account::<SplTokenAccount>(svm, account)
        .expect("token account should exist")
        .amount
}

/// Advance the on-chain clock by `secs` seconds.
fn warp(svm: &mut LiteSVM, secs: i64) {
    let mut clock: Clock = svm.get_sysvar();
    clock.unix_timestamp += secs;
    svm.set_sysvar(&clock);
}

struct World {
    svm: LiteSVM,
    admin: Keypair,
    stake_mint: Pubkey,
    reward_mint: Pubkey,
    pool: Pubkey,
    stake_vault: Pubkey,
    reward_vault: Pubkey,
}

/// Boot a LiteSVM with the program loaded, two SPL mints created, and the pool
/// initialized.
fn setup() -> World {
    let mut svm = LiteSVM::new();
    svm.add_program(PROGRAM_ID, PROGRAM_SO).unwrap();

    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100 * SOL).unwrap();

    let stake_mint = CreateMint::new(&mut svm, &admin)
        .decimals(DECIMALS)
        .send()
        .unwrap();
    let reward_mint = CreateMint::new(&mut svm, &admin)
        .decimals(DECIMALS)
        .send()
        .unwrap();

    let pool = Pubkey::find_program_address(&[b"pool", stake_mint.as_ref()], &PROGRAM_ID).0;
    let stake_vault = ata(&pool, &stake_mint);
    let reward_vault = ata(&pool, &reward_mint);

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(stake_mint, false),
            AccountMeta::new_readonly(reward_mint, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(reward_vault, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new_readonly(ATA_PROGRAM, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data: ix_data::InitializePool {
            reward_rate: REWARD_RATE,
        }
        .data(),
    };
    send(&mut svm, ix, &admin, &[&admin]).unwrap();

    World {
        svm,
        admin,
        stake_mint,
        reward_mint,
        pool,
        stake_vault,
        reward_vault,
    }
}

/// Admin mints `amount` reward tokens and funds the reward vault.
fn fund_rewards(w: &mut World, amount: u64) {
    let admin_reward = CreateAssociatedTokenAccount::new(&mut w.svm, &w.admin, &w.reward_mint)
        .owner(&w.admin.pubkey())
        .send()
        .unwrap();
    MintTo::new(&mut w.svm, &w.admin, &w.reward_mint, &admin_reward, amount)
        .owner(&w.admin)
        .send()
        .unwrap();

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(w.admin.pubkey(), true),
            AccountMeta::new_readonly(w.pool, false),
            AccountMeta::new(w.reward_vault, false),
            AccountMeta::new(admin_reward, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ],
        data: ix_data::FundRewards { amount }.data(),
    };
    let admin = w.admin.insecure_clone();
    send(&mut w.svm, ix, &admin, &[&admin]).unwrap();
}

/// Create a funded user with a stake-token account holding `mint_amount`.
fn make_user(w: &mut World, mint_amount: u64) -> (Keypair, Pubkey, Pubkey) {
    let user = Keypair::new();
    w.svm.airdrop(&user.pubkey(), 100 * SOL).unwrap();
    let user_stake = CreateAssociatedTokenAccount::new(&mut w.svm, &user, &w.stake_mint)
        .owner(&user.pubkey())
        .send()
        .unwrap();
    MintTo::new(&mut w.svm, &w.admin, &w.stake_mint, &user_stake, mint_amount)
        .owner(&w.admin)
        .send()
        .unwrap();
    let stake_account = Pubkey::find_program_address(
        &[b"stake", w.pool.as_ref(), user.pubkey().as_ref()],
        &PROGRAM_ID,
    )
    .0;
    (user, user_stake, stake_account)
}

fn stake_ix(w: &World, user: &Keypair, user_stake: &Pubkey, stake_account: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(w.pool, false),
            AccountMeta::new(*stake_account, false),
            AccountMeta::new(w.stake_vault, false),
            AccountMeta::new(*user_stake, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data: ix_data::Stake { amount }.data(),
    }
}

fn unstake_ix(w: &World, user: &Keypair, user_stake: &Pubkey, stake_account: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(w.pool, false),
            AccountMeta::new(*stake_account, false),
            AccountMeta::new_readonly(user.pubkey(), false), // owner
            AccountMeta::new(w.stake_vault, false),
            AccountMeta::new(*user_stake, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ],
        data: ix_data::Unstake { amount }.data(),
    }
}

fn claim_ix(w: &World, user: &Keypair, user_reward: &Pubkey, stake_account: &Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new_readonly(w.pool, false),
            AccountMeta::new(*stake_account, false),
            AccountMeta::new_readonly(user.pubkey(), false), // owner
            AccountMeta::new(w.reward_vault, false),
            AccountMeta::new(*user_reward, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ],
        data: ix_data::Claim {}.data(),
    }
}

#[test]
fn full_lifecycle() {
    let mut w = setup();
    fund_rewards(&mut w, 1_000_000);
    assert_eq!(token_balance(&w.svm, &w.reward_vault), 1_000_000);

    let (user, user_stake, stake_account) = make_user(&mut w, 5_000_000);

    // Stake 1_000_000.
    let ix = stake_ix(&w, &user, &user_stake, &stake_account, 1_000_000);
    send(&mut w.svm, ix, &user, &[&user]).unwrap();
    assert_eq!(token_balance(&w.svm, &w.stake_vault), 1_000_000);
    assert_eq!(token_balance(&w.svm, &user_stake), 4_000_000);

    // Accrue for 100s: reward = 1_000_000 * 100 / 1_000_000 = 100.
    warp(&mut w.svm, 100);
    let user_reward = CreateAssociatedTokenAccount::new(&mut w.svm, &user, &w.reward_mint)
        .owner(&user.pubkey())
        .send()
        .unwrap();
    let ix = claim_ix(&w, &user, &user_reward, &stake_account);
    send(&mut w.svm, ix, &user, &[&user]).unwrap();
    assert_eq!(token_balance(&w.svm, &user_reward), 100);

    // Accrue another 50s on the full stake, then unstake part of it.
    warp(&mut w.svm, 50);
    let ix = unstake_ix(&w, &user, &user_stake, &stake_account, 400_000);
    send(&mut w.svm, ix, &user, &[&user]).unwrap();
    assert_eq!(token_balance(&w.svm, &w.stake_vault), 600_000);
    assert_eq!(token_balance(&w.svm, &user_stake), 4_400_000);

    // The 50s accrual (1_000_000 * 50 / 1_000_000 = 50) is now claimable.
    let ix = claim_ix(&w, &user, &user_reward, &stake_account);
    send(&mut w.svm, ix, &user, &[&user]).unwrap();
    assert_eq!(token_balance(&w.svm, &user_reward), 150);
}

#[test]
fn rejects_zero_stake() {
    let mut w = setup();
    let (user, user_stake, stake_account) = make_user(&mut w, 1_000_000);
    let ix = stake_ix(&w, &user, &user_stake, &stake_account, 0);
    assert!(send(&mut w.svm, ix, &user, &[&user]).is_err());
}

#[test]
fn rejects_overdraw_unstake() {
    let mut w = setup();
    let (user, user_stake, stake_account) = make_user(&mut w, 1_000_000);
    let ix = stake_ix(&w, &user, &user_stake, &stake_account, 1_000_000);
    send(&mut w.svm, ix, &user, &[&user]).unwrap();

    let ix = unstake_ix(&w, &user, &user_stake, &stake_account, 2_000_000);
    assert!(send(&mut w.svm, ix, &user, &[&user]).is_err());
}

#[test]
fn rejects_empty_claim() {
    let mut w = setup();
    fund_rewards(&mut w, 1_000_000);
    let (user, user_stake, stake_account) = make_user(&mut w, 1_000_000);
    let ix = stake_ix(&w, &user, &user_stake, &stake_account, 1_000_000);
    send(&mut w.svm, ix, &user, &[&user]).unwrap();

    // No time has passed: nothing accrued, so claim must reject.
    let user_reward = CreateAssociatedTokenAccount::new(&mut w.svm, &user, &w.reward_mint)
        .owner(&user.pubkey())
        .send()
        .unwrap();
    let ix = claim_ix(&w, &user, &user_reward, &stake_account);
    assert!(send(&mut w.svm, ix, &user, &[&user]).is_err());
}
