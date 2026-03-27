# Rewards and the Epoch Boundary {#sec:epoch}

This chapter introduces the epoch boundary transition system and the
related reward calculation.

The transition system is defined in
Section [1.8](#sec:total-epoch){reference-type="ref"
reference="sec:total-epoch"}, and involves taking stake distribution
snapshots (Sections [1.4](#sec:stake-dist-calc){reference-type="ref"
reference="sec:stake-dist-calc"}
and [1.5](#sec:snapshots){reference-type="ref"
reference="sec:snapshots"}), retiring stake pools
(Section [1.6](#sec:pool-reap){reference-type="ref"
reference="sec:pool-reap"}), and performing protocol updates
(Section [1.7](#sec:pparam-update){reference-type="ref"
reference="sec:pparam-update"}). The reward calculation, defined in
Sections [1.9](#sec:reward-dist){reference-type="ref"
reference="sec:reward-dist"}
and [1.10](#sec:reward-calc){reference-type="ref"
reference="sec:reward-calc"}, distributes the leader election rewards.

## Overview of the Reward Calculation {#sec:reward-overview}

The rewards for a given epoch $e_i$ involve the two epochs surrounding
it. In particular, the stake distribution will come from the previous
epoch and the rewards will be calculated in the following epoch. More
concretely:

(A) A stake distribution snapshot is taken at the begining of epoch
    $e_{i-1}$.

(B) The randomness for leader election is fixed during epoch $e_{i-1}$

(C) Epoch $e_{i}$ begins.

(D) Epoch $e_{i}$ ends. A snapshot is taken of the stake pool
    performance during epoch $e_{i}$. A snapshot is also taken of the
    fee pot.

(E) The snapshots from (D) are stable and the reward calculation can
    begin.

(F) The reward calculation is finished and an update to the ledger state
    is ready to be applied.

(G) Rewards are given out.

We must therefore store the last three stake distributions. The mnemonic
"mark, set, go" will be used to keep track of the snapshots, where the
label "mark" refers to the most recent snapshot, and "go" refers to the
snapshot that is ready to be used in the reward calculation. In the
above diagram, the snapshot taken at (A) is labeled "mark" during epoch
$e_{i-1}$, "set" during epoch $e_i$ and "go" during epoch $e_{i+1}$. At
(G) the snapshot taken at (A) is no longer needed and will be discarded.

The two main transition systems in this section are:

- The transition system named $\mathsf{EPOCH}$, which is defined in
  Section [1.8](#sec:total-epoch){reference-type="ref"
  reference="sec:total-epoch"}, covers what happens at the epoch
  boundary, such as at (A), (C), (D) and (G).

- The transition named $\mathsf{RUPD}$, which is defined in
  Section [\[sec:reward-update-trans\]](#sec:reward-update-trans){reference-type="ref"
  reference="sec:reward-update-trans"}, covers the reward calculation
  that happens between (E) and (F).

::: note
Between time D and E we are concerned with chain growth and stability.
Therefore this duration can be stated as 2k blocks (to state it in slots
requires details about the particular version of the Ouroboros
protocol). The duration between F and G is also 2k blocks. Between E and
F a single honest block is enough to ensure a random nonce.
:::

## Example Illustration of the Reward Cycle {#sec:illustration-reward-cycle}

1.00,0.50,0.00 0.65,0.00,0.00 0.00,0.50,0.00 0.00,0.95,0.00
0.00,0.00,0.90 0.00,0.60,0.90

Bob registers his stake pool in epoch $e_1$. Alice delegates to Bob's
stake pool in epoch $e_1$. Just before the end of epoch $e_1$, Bob
submits a stake pool re-registration, changing his pool parameters. The
change in parameters is not immediate, as shown by the curved arrow
around the epoch boundary.

A snapshot is taken on the $e_1$/$e_2$ boundary. It is labeled "mark"
initially. This snapshot includes Alice's delegation to Bob's pool, and
Bob's pool parameters and listed in the initial pool registration
certificate.

If Alice changes her delegation choice any time during epoch $e_2$, she
will never be effected by Bob's change of parameters.

A new snapshot is taken on the $e_2$/$e_3$ boundary. The previous
(darker blue) snapshot is now labeled "set", and the new one labeled
"mark". The "set" snapshot is used for leader election in epoch $e_3$.

On the $e_3$/$e_4$ boundary, the darker blue snapshot is labeled "go"
and the lighter blue snapshot is labeled "set". Bob's stake pool
performance during epoch $e_3$ (he produced 4 blocks) will be used with
the darker blue snapshot for the rewards which will be handed out at the
beginning of epoch $e_5$.
