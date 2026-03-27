# Haskell Ledger Oracle Memory

## Key File Locations in cardano-ledger

### EPOCH STS Rule
- `eras/shelley/impl/src/Cardano/Ledger/Shelley/Rules/Epoch.hs`
- Sub-rule ordering: SNAP then POOL (but Haskell applies POOL *before* SNAP wrt snapshot content)
- Pool retirement condition: `retirement_epoch <= new_epoch` equivalent → retire when cert epoch == self.epoch

### NEWEPOCH STS Rule
- `eras/shelley/impl/src/Cardano/Ledger/Shelley/Rules/NewEpoch.hs`
- `nesBprev` = previous epoch's block production map (used by RUPD as actual blocks)

### Reward Update (RUPD / PulsingReward)
- `libs/cardano-ledger-core/src/Cardano/Ledger/Rewards.hs`
- `eras/shelley/impl/src/Cardano/Ledger/Shelley/RewardUpdate.hs`
- `eras/shelley/impl/src/Cardano/Ledger/Shelley/Rules/Rupd.hs`
- blocksTotal passed to mkApparentPerformance = actual total blocks (fromIntegral $ Map.foldr (+) 0 b')

### Stake Snapshots
- `libs/cardano-ledger-core/src/Cardano/Ledger/EpochBoundary.hs`
- spssStake = domRestrictedMap (delegations ▷ dom poolParams) (utxo ∪+ rewards)
- Only credentials delegated to ACTIVE pools included

### Pool Retirement
- POOL STS runs in EPOCH STS before SNAP STS
- retiring at epoch N = active through N, mark at N+1 won't have the pool
- Haskell condition: `Map.filterWithKey (\e _ -> e <= epochNo) retirements`

## Confirmed Behaviors

### Pool retirement timing
Retirement cert specifying epoch N: pool active through N, retired at N→(N+1) boundary.
POOL rule applies retirement BEFORE SNAP takes the mark snapshot.
Confirmed from epoch dump evidence: pool bd602b4f with retirement epoch 583 had 292 blocks in epoch 583
and was absent from mark.poolParams at epoch 584.

### nesBprev / block counting
`nesBprev` is the complete block map from the previous epoch. It is used unchanged as the
`blocksTotal` parameter for apparent performance. This means go_snapshot.epoch_blocks_by_pool
in hayate must = blocks from the epoch 3 epochs prior.

## Conway Governance (cardano-ledger Conway STS rules)

### Key Files
- `eras/conway/impl/src/Cardano/Ledger/Conway/Rules/NewEpoch.hs` — NEWEPOCH rule
- `eras/conway/impl/src/Cardano/Ledger/Conway/Rules/Epoch.hs` — EPOCH rule
- `eras/conway/impl/src/Cardano/Ledger/Conway/Rules/Ratify.hs` — RATIFY rule
- `eras/conway/impl/src/Cardano/Ledger/Conway/Rules/Enact.hs` — ENACT rule
- `eras/conway/impl/src/Cardano/Ledger/Conway/Rules/Gov.hs` / `GovCert.hs` / `Deleg.hs`
- `eras/conway/impl/src/Cardano/Ledger/Conway/Governance/DRepPulser.hs` — drepDistr computation
- `eras/conway/impl/src/Cardano/Ledger/Conway/Governance/Internal.hs`

### NEWEPOCH STS Order (Conway)
1. Extract/complete RUPD — apply rewards to accounts → `es1`
2. EPOCH sub-rule → `es2` (SNAP, POOLREAP, extract ratify state, enactment, deposit returns, HARDFORK, **setFreshDRepPulsingState**)
3. Pool distribution: `pd' = ssStakeMarkPoolDistr (esSnapshots es0)` — from **es0** (pre-reward state!)
4. Update NewEpochState

### EPOCH Sub-rule Order (Conway)
1. **SNAP** — rotate snapshots, take new mark from current state (post-reward)
2. **POOLREAP** — retire pools
3. **Extract RatifyState** from DRep pulser (pulsed during epoch, completed here): `rsEnacted`, `rsExpired`
4. **applyEnactedWithdrawals** — apply treasury withdrawals from EnactState to accounts
5. **proposalsApplyEnactment** — remove enacted/expired proposals from proposal tree
6. **Update GovState** — new committee/constitution/params (`nextEpochPParams → curPParams`, `curPParams → prevPParams`), clear futurePParams
7. **returnProposalDeposits** — deposit returns to reward_accounts (or treasury if unregistered)
8. Update CertState (increment dormant epoch counter)
9. **HARDFORK** if curPv ≠ prevPv
10. **setFreshDRepPulsingState** — LAST STEP: snapshot current state for next epoch's ratification

### drepDistr Computation (`computeDRepDistr` in DRepPulser.hs)
```
drepDistr[drep] += instantStake[cred] + proposalDeposits[cred] + rewardBalance[cred]
```
for each `cred` in `Accounts` where `casDRepDelegation = SJust drep`.

- `instantStake` = UTxO-derived stake for ALL credentials (not pool-delegation restricted)
- `proposalDeposits` = sum of governance proposal deposits whose return addr = cred
- `rewardBalance` = reward account balance (`casBalance`)
- For `DRepCredential cred`: only if `cred ∈ vsDReps` (DRep is registered); if unregistered, skip
- For `DRepAlwaysAbstain`/`DRepAlwaysNoConfidence`: no registration check needed

**TIMING**: The drepDistr used for RATIFY at epoch N→N+1 was snapshotted (via pulser init) at epoch N-1→N. So ratification always uses the PREVIOUS epoch's distribution.

**The epoch N dump `drepDistr` shows the freshly-snapshotted distribution (for N+1 ratification)**, i.e. snapshotted at end of epoch N→N+1 transition (step 10 = setFreshDRepPulsingState).

### RATIFY Rule (ratifyTransition)
Processes proposals in priority order: NoConfidence(0) > UpdateCommittee(1) > NewConstitution(2) > HardForkInitiation(3) > ParameterChange(4) > TreasuryWithdrawals(5) > InfoAction(6).

Ratification requires ALL of:
1. `prevActionAsExpected` — parent gov action ID matches last enacted root
2. `validCommitteeTerm` — for UpdateCommittee, new term ≤ maxTermLength
3. `not rsDelayed` — no "delaying action" already ratified this epoch
4. `withdrawalCanWithdraw` — for TreasuryWithdrawals, amount ≤ treasury
5. `acceptedByEveryone`: CC + SPO + DRep all above thresholds

Bootstrap phase (PV=9): all DRep thresholds = 0 (auto-pass).

### DRep Accepted Ratio (`dRepAcceptedRatio`)
Uses `reDRepDistr` (the snapshotted drepDistr, NOT live state). Iterates:
- `DRepCredential cred`: skip if unregistered OR expired. No vote = counted in denominator only. Abstain = excluded entirely. Yes/No = counted normally.
- `DRepAlwaysNoConfidence`: auto-Yes on NoConfidence, auto-No on all others. Always in denominator.
- `DRepAlwaysAbstain`: never counted in numerator or denominator.

Formula: `yesStake / totalExcludingAbstainStake ≥ threshold`

### SPO Accepted Ratio (`spoAcceptedRatio`)
Uses `reStakePoolDistr` (mark pool distribution snapshot from NEWEPOCH step 3).
Default vote when SPO doesn't vote:
- `HardForkInitiation`: No
- Bootstrap phase: Abstain
- Otherwise: check operator reward account DRep delegation:
  - `DRepAlwaysAbstain` → Abstain
  - `DRepAlwaysNoConfidence` → Yes (on NoConfidence), No (on others)
  - else → No

### Committee Accepted Ratio (`committeeAcceptedRatio`)
`yes / (yes + no)` — excludes: abstains, expired members, members with no hot key, resigned members.
Members who haven't voted = counted as No.
`committeeMinSize` check after bootstrap.

### Deposits in EPOCH Rule
- **Enacted proposals**: deposit returned at `returnProposalDeposits` (step 7 of EPOCH), which is AFTER SNAP and AFTER enactment. Deposit appears in accounts before `setFreshDRepPulsingState`.
- **Expired proposals**: also returned at `returnProposalDeposits` (same step, same timing).
- If return address not registered: deposit goes to treasury (not lost).

### DRep Registration/Delegation (GovCert.hs, Deleg.hs)
- `ConwayRegDRep`: adds to `vsDReps` with empty `drepDelegs` set
- `DelegVote`/`DelegStakeVote`: sets `casDRepDelegation = SJust dRep` on account, adds cred to `drepDelegs` set
- `ConwayUnRegDRep`: removes from `vsDReps` AND clears `dRepDelegationAccountStateL` for all delegators in `drepDelegs`

