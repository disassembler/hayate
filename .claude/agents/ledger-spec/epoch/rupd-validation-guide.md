# RUPD Validation Guide

> **TL;DR — just want to check the transition?** Run this instead of reading the rest:
> ```
> ./target/release/compare-epoch-dumps rupd-trace <epoch_N.json> <epoch_N+1.json> --expected-blocks 4320
> ```
> (preview: `--expected-blocks 4320`; mainnet: `--expected-blocks 21600`)
>
> The rest of this file is context for answering questions about the formulas and
> process — use it when the script flags a mismatch and you need to understand why.

---

## Dump JSON → Spec Variable Mapping

### From epoch N dump (rupdNext)
| JSON field | Spec variable | Notes |
|---|---|---|
| `rupdNext.deltaT1` | Δt₁ | Treasury increment |
| `rupdNext.deltaR1` | Δr₁ | Reserves decrement |
| `rupdNext.deltaR2` | Δr₂ | Unused rewards returned to reserves |
| `rupdNext.rewardPayouts` | rs | Map credential→coin of actual payouts |
| `rupdNext.unregisteredRewardAcnts` | unregRU' | Rewards for accounts deregistered after RUPD creation |
| `rupdNext.totalDistributed` | Σrs | Sum of rewardPayouts values |
| `rupdNext.feeSS` | feeSS | Fee pot at snapshot time |

### From epoch N snapshots (go snapshot — used for reward calculation)
| JSON field | Spec variable | Notes |
|---|---|---|
| `snapshots.go.stake` | σ (stake distribution) | Credential→lovelace |
| `snapshots.go.delegations` | delegs | Credential→pool hash |
| `snapshots.go.poolParameters` | poolParams | Pool hash→PoolParam |
| `snapshots.go.activeStake` | total active stake | Sum of all go.stake values |
| `snapshots.go.blocksByPool` | b (blocks made) | Pool hash→block count; used as numerator in pool reward |

> **Note**: `epoch_blocks_by_pool` (mark snapshot) = blocks made during the
> current epoch. `go.blocksByPool` = blocks from 2 epochs ago, already
> rotated to `go` by the time rewards are calculated.

### From epoch N protocol params
| JSON field | Spec variable |
|---|---|
| `protocolParams.decentralisationParam` or `d` | d |
| `protocolParams.monetaryExpansion` or `rho` (ρ) | ρ |
| `protocolParams.treasuryCut` or `tau` (τ) | τ |
| `protocolParams.poolPledgeInfluence` or `a0` | a₀ |
| `protocolParams.nOpt` or `k` | k |
| `protocolParams.maxBlockBodySize` etc. | — |

### From epoch N accounting
| JSON field | Spec variable |
|---|---|
| `accountState.treasury` | treasury (before apply) |
| `accountState.reserves` | reserves (before apply) |

---

## RUPD Formula Chain (in order)

Given epoch N's dump, RUPD is *created* using go snapshot blocks and
*applied* at epoch N+1 transition.

```
1.  η      = sum(go.blocksByPool) / expectedBlocks
              where expectedBlocks = slots_per_epoch * (1 - d)
              (in Shelley/early eras d=0 so expected = slots_per_epoch / active_slots_coeff)
              if η > 1, use 1 (min cap)

2.  Δr₁    = floor(min(1, η) · ρ · reserves_N)

3.  feeSS  = (from rupdNext.feeSS; the fee pot at snapshot time)

4.  rPot   = feeSS + Δr₁                    (total reward pot)

5.  Δt₁    = floor(τ · rPot)                (treasury cut)

6.  R      = rPot - Δt₁                     (available for pools+delegators)

7.  rs     = rewardOnePool(...) for each pool, summed per credential
             = {} when all pool reward accounts are unregistered, or all
               pools have margin=1, or pledge not met, etc.

8.  Δr₂   = R - Σrs                         (unspent pool rewards → reserves)

9.  Apply:
      treasury_{N+1} = treasury_N + Δt₁ + unregRU'
      reserves_{N+1} = reserves_N - Δr₁ + Δr₂
      rewards[cred] += rs[cred]  (for each registered credential)
```

---

## Epoch N → N+1 Carry-Forward Verification

After reading both dumps, verify:

```
treasury_{N+1} == treasury_N + rupdNext.deltaT1 + rupdNext.unregisteredRewardAcnts_sum
reserves_{N+1} == reserves_N - rupdNext.deltaR1 + rupdNext.deltaR2
```

Also: `rupdNext` in epoch N **must exactly equal** `rupdApplied` in epoch N+1.
If they differ, hayate produced the wrong RUPD or applied it incorrectly.

---

## Why rs = {} (Zero Pool Rewards)

The three most common causes, in rough order of frequency:

1. **Unregistered reward account** — pool's `rewardAccount` (the `ra` field in
   poolParameters) is not in `rewards` map at RUPD creation time. The pool is
   excluded from `rewardOnePool` because `addrs_rew = dom(rewards)` filters it.
   → All R goes to Δr₂ (reserves).

2. **margin = 1** — operator takes 100% of the pool reward; member share is
   `r_member = max(0, r - r_leader) = 0`. Pool operator reward is also 0 unless
   the reward account is registered. If the account is registered you'd see a
   non-zero entry for the operator credential.

3. **Pledge not met** — if `stake(owner) < pledge`, the pool is excluded
   entirely from the reward calculation.

4. **No blocks produced** — pool gets 0 reward trivially (b_pool = 0 in
   `rewardOnePool`).

---

## rewardOnePool Quick Reference

```
σ_pool  = poolStake(hk, delegs, stake)   -- filtered stake for this pool
σ       = total active stake (go.activeStake)
b_pool  = go.blocksByPool[hk]            -- blocks produced this epoch
n       = total blocks (sum go.blocksByPool)
s       = pledge                          -- pool's declared pledge
a₀, k   = protocol params

maxPool = (R / (1 + a₀)) · (σ'/σ + (a₀ · s'/σ) · (σ'/σ - s'·(z₀-σ')/z₀))
  where z₀ = 1/k, s' = min(s,z₀), σ' = min(σ_pool/σ, z₀)

apparentPerf = b_pool / n   (NO min(1) cap — Haskell does not cap this)

pool_reward = floor(apparentPerf · totalActiveStake/σ_pool · maxPool)
  (hayate formula, confirmed matching Haskell)

r_leader = min(pool_reward, margin · pool_reward + (1-margin)·pool_reward·s/σ_pool)
r_member(cred) = floor((1-margin) · pool_reward · stake(cred)/σ_pool)
               = 0 when margin=1
```

---

## Reading a Dump Pair Quickly

1. Open epoch N dump → note `rupdNext.*`, `accountState.*`, `snapshots.go.*`
2. Open epoch N+1 dump → note `rupdApplied.*`, `accountState.*`
3. Check `rupdNext == rupdApplied` (field by field)
4. Compute treasury and reserves carry-forward and compare to epoch N+1 actuals
5. If rs={}: check pool reward accounts against `rewardAccounts` keys to confirm exclusion reason
6. If rs≠{}: pick one pool, run rewardOnePool manually as a spot check
