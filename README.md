# anchor-staking-rewards

A token staking vault with time-based rewards, written in Anchor.

Stake an SPL token into a program-owned vault and accrue a second SPL token as rewards, proportional to how much you stake and how long it stays staked. It is a compact, fully-tested reference of the kind of program that sits underneath a yield product, kept small so the mechanics are easy to read.

## Reward model

```
reward = staked_amount * elapsed_seconds * reward_rate / 1e12
```

`reward_rate` is reward tokens per staked token per second, scaled by a fixed-point factor of `1e12`. Rewards are **settled on every balance change** (stake, unstake, claim): the accrual since the last settlement is banked into the account before the balance moves, so changing your stake never retroactively reprices past accrual. All arithmetic is checked for overflow.

## Instructions

| Instruction | Caller | Effect |
| --- | --- | --- |
| `initialize_pool(reward_rate)` | admin | Creates the pool and its stake + reward vaults (ATAs owned by the pool PDA). |
| `fund_rewards(amount)` | admin | Tops up the reward vault so the pool can pay claims. |
| `stake(amount)` | user | Deposits stake tokens into the vault; settles pending rewards first. |
| `unstake(amount)` | user | Withdraws stake tokens; settles pending rewards first. |
| `claim()` | user | Mints out all accrued rewards from the reward vault. |

## Accounts and PDAs

- **Pool** (`["pool", stake_mint]`): admin, the two mints, both vaults, `reward_rate`, `total_staked`.
- **StakeAccount** (`["stake", pool, user]`): owner, balance, banked `reward_debt`, `last_update` timestamp.
- **Vaults**: associated token accounts owned by the pool PDA. The pool signs vault releases with its PDA seeds.

Each stake account is locked to its owner through a `has_one = owner` constraint plus an address check, so one user can never settle or drain another user's position.

## Testing

The tests load the **actual compiled program** into [LiteSVM](https://github.com/LiteSVM/litesvm) and drive it exactly as a client would: real SPL mints, real PDAs, and a warped clock to exercise time-based accrual.

```bash
anchor build      # produces target/deploy/anchor_staking_rewards.so
cargo test        # runs tests/staking.rs against that .so
```

`target/` is gitignored, so run `anchor build` before `cargo test`: the tests `include_bytes!` the compiled `.so`.

The suite (`tests/staking.rs`, 4 tests) covers:

- **`full_lifecycle`** - fund, stake, accrue 100s and claim exactly 100, accrue 50s more, partial unstake, then claim the remaining 50. Reward totals are asserted to the lamport.
- **`rejects_zero_stake`** - staking zero is rejected.
- **`rejects_overdraw_unstake`** - unstaking more than the balance is rejected.
- **`rejects_empty_claim`** - claiming with nothing accrued is rejected.

```
test result: ok. 4 passed; 0 failed
```

## Design notes

- **Settle-on-change vs. global accumulator.** This program settles each account on its own writes, which is simple and exact for a moderate number of stakers. A pool serving very many stakers would instead track a global reward-per-token accumulator and have each account snapshot it, trading a little complexity for O(1) settlement regardless of staker count.
- **Fixed-point.** Reward math runs in `u128` with a `1e12` precision factor, then narrows back to `u64`, so small per-second rates do not truncate to zero.

## Status

Devnet-grade reference, unaudited, not for production without review. `reward_rate` is fixed at pool creation, and the vault only ever releases to stakers (no admin path to withdraw staked principal).

## Stack

Anchor 1.x, Rust, SPL Token, LiteSVM.
