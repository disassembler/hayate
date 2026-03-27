## Rewards Distribution Calculation {#sec:reward-dist}

This section defines the reward calculation for the proof of stake
leader election.
Figure [13](#fig:functions:rewards){reference-type="ref"
reference="fig:functions:rewards"} defines the pool reward as described
in section 5.5.2 of [@delegation_design].

- The function $\fun{maxPool}$ gives the maximum reward a stake pool can
  receive in an epoch. This is a fraction of the total available rewards
  for the epoch. The result depends on the pool's relative stake, the
  pool's pledge and the following protocol parameters:

  - $\var{a_0}$, the leader-stake influence

  - $n_{opt}$, the optimal number of saturated stake pools

- The function $\fun{mkApparentPerformance}$ computes the apparent
  performance of a stake pool. It depends on the protocol parameter $d$,
  the relative stake $\sigma$, the number $n$ of blocks the pool added
  to the chain and the total number $\overline{N}$ of blocks added to
  the chain in the last epoch.

<figure id="fig:functions:rewards">
<p><em>Maximal Reward Function, called <span
class="math inline"><em>f</em>(<em>s</em>, <em>σ</em>)</span> in section
5.5.2 of <span class="citation"
data-cites="delegation_design"></span></em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{maxPool} \in \PParams \to \Coin \to \unitInterval \to
\unitInterval \to \Coin \\
      &amp; \fun{maxPool}~\var{pp}~\var{R}~\sigma~\var{p_r} =
          ~~~\floor*{
             \frac{R}{1 + a_0}
             \cdot
             \left(
               \sigma' + p'\cdot a_0\cdot\frac{\sigma' -
p'\frac{z_0-\sigma'}{z_0}}{z_0}
             \right)} \\
      &amp; ~~~\where \\
      &amp; ~~~~~~~a_0 = \fun{influence}~pp \\
      &amp; ~~~~~~~n_{opt} = \fun{nopt}~pp \\
      &amp; ~~~~~~~z_0 = 1/n_{opt} \\
      &amp; ~~~~~~~\sigma'=\min(\sigma,~z_0) \\
      &amp; ~~~~~~~p'=\min(p_r,~z_0) \\
  
\end{aligned}$$</span></p>
<p><em>Apparent Performance, called <span
class="math inline"><em>p̂</em></span> in section 5.5.2 of <span
class="citation" data-cites="delegation_design"></span></em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{mkApparentPerformance} \in \unitInterval \to
\unitInterval \to \N \to \N \to \Q \\
      &amp;
\fun{mkApparentPerformance}~\var{d}~{\sigma}~\var{n}~\var{\overline{N}}
=
        \begin{cases}
          \frac{\beta}{\sigma} &amp; \text{if } d &lt; 0.8 \\
          1 &amp; \text{otherwise}
        \end{cases} \\
      &amp; ~~~\where \\
      &amp; ~~~~~~~\beta = \frac{n}{\max(1, \overline{N})} \\
  
\end{aligned}$$</span></p>
<figcaption>Functions used in the Reward Calculation</figcaption>
</figure>

Figure [14](#fig:functions:reward-splitting){reference-type="ref"
reference="fig:functions:reward-splitting"} gives the calculation for
splitting the pool rewards with its members, as described in 6.5.2 of
[@delegation_design]. The portion of rewards allocated to the pool
operator and owners is different than that of the members.

- The $\fun{r_{operator}}$ function calculates the leader reward, based
  on the pool cost, margin and the proportion of the pool's total stake.
  Note that this reward will go to the reward account specified in the
  pool registration certificate.

- The $\fun{r_{member}}$ function calculates the member reward,
  proportionally to their stake after the cost and margin are removed.

<figure id="fig:functions:reward-splitting">
<p><em>Pool leader reward, from section 5.5.3 of <span class="citation"
data-cites="delegation_design"></span></em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{r_{operator}} \in \Coin \to \PoolParam \to
\unitInterval \to \unitIntervalNonNull \to \Coin \\
      &amp; \fun{r_{operator}}~ \var{\hat{f}}~ \var{pool}~ \var{s}~
{\sigma} =
        \begin{cases}
        \hat{f} &amp; \hat{f} \leq c\\
        c + \floor*{(\hat{f} - c)\cdot\left(m +
(1-m)\cdot\frac{s}{\sigma}\right) }&amp;
        \text{otherwise.}
      \end{cases} \\
      &amp; ~~~\where \\
      &amp; ~~~~~~~c = \fun{poolCost}~pool \\
      &amp; ~~~~~~~m = \fun{poolMargin}~pool \\
  
\end{aligned}$$</span></p>
<p><em>Pool member reward, from section 5.5.3 of <span class="citation"
data-cites="delegation_design"></span></em> <span
class="math display">$$\begin{aligned}
    &amp; \fun{r_{member}} \in \Coin \to \PoolParam \to \unitInterval
\to \unitIntervalNonNull \to \Coin \\
    &amp; \fun{r_{member}}~ \var{\hat{f}}~ \var{pool}~ \var{t}~ {\sigma}
=
      \begin{cases}
        0 &amp; \hat{f} \leq c\\
        \floor*{(\hat{f} - c)\cdot(1-m)\cdot\frac{t}{\sigma}} &amp;
        \text{otherwise.}
      \end{cases} \\
    &amp; ~~~\where \\
    &amp; ~~~~~~~c = \fun{poolCost}~pool \\
    &amp; ~~~~~~~m = \fun{poolMargin}~pool \\
  
\end{aligned}$$</span></p>
<figcaption>Functions used in the Reward Splitting</figcaption>
</figure>

Finally, the full reward calculation is presented in
Figure [15](#fig:functions:reward-calc){reference-type="ref"
reference="fig:functions:reward-calc"}. The calculation is done
pool-by-pool.

- The $\fun{rewardOnePool}$ function calculates the rewards given out to
  each member of a given pool. The pool leader is identified by the
  stake credential of the pool operator. The function returns the
  rewards, calculated as follows:

  - $\var{pstake}$, the total amount of stake controlled by the stake
    pool.

  - $\var{ostake}$, the total amount of stake controlled by the stake
    pool operator and owners

  - $\sigma$, the total proportion of stake controlled by the stake
    pool.

  - $\overline{N}$, the expected number of blocks the pool should have
    produced.

  - $\var{pledge}$, the pool's pledge in lovelace.

  - $p_r$, the pool's pledge, as a proportion of active stake.

  - $\var{maxP}$, maximum rewards the pool can claim if the pledge is
    met, and zero otherwise.

  - $\var{poolR}$, the pool's actual reward, based on its performance.

  - $\var{mRewards}$, the member's rewards as a mapping of reward
    accounts to coin.

  - $\var{lReward}$, the leader's reward as coin.

  - $\var{potentialRewards}$, the combination of $\var{mRewards}$ and
    $\var{lRewards}$.

  - $\var{rewards}$, the restriction of $\var{potentialRewards}$ to the
    active reward accounts.

- The $\fun{reward}$ function applies $\fun{rewardOnePool}$ to each
  registered stake pool.

<figure id="fig:functions:reward-calc">
<p><em>Calculation to reward a single stake pool</em> <span
class="math display">$$\begin{aligned}
    &amp; \fun{rewardOnePool} \in \PParams \to \Coin \to \N \to \N \to
\PoolParam\\
      &amp; ~~~\to \type{Stake}\to \Q \to \Q \to \Coin \to
\powerset{\AddrRWD}
           \to (\AddrRWD \mapsto \Coin) \\
      &amp;
\fun{rewardOnePool}~\var{pp}~\var{R}~\var{n}~\var{\overline{N}}~\var{pool}~\var{stake}~{\sigma}~{\sigma_a}~\var{tot}~\var{addrs_{rew}}
=
          \var{rewards}\\
      &amp; ~~~\where \\
      &amp; ~~~~~~~\var{ostake} = \sum_{\substack{
        hk_\mapsto c\in\var{stake}\\
        hk\in(\fun{poolOwners}~\var{pool})\\
        }} c \\
      &amp; ~~~~~~~\var{pledge} = \fun{poolPledge}~pool \\
      &amp; ~~~~~~~p_{r} = \var{pledge} / \var{tot} \\
      &amp; ~~~~~~~maxP =
      \begin{cases}
        \fun{maxPool}~\var{pp}~\var{R}~\sigma~\var{p_r}&amp;
        \var{pledge} \leq \var{ostake}\\
        0 &amp; \text{otherwise.}
      \end{cases} \\
      &amp; ~~~~~~~\var{appPerf} =
\fun{mkApparentPerformance}~\var{(\fun{d}~pp)}~{\sigma_a}~\var{n}~\var{\overline{N}}
\\
      &amp; ~~~~~~~\var{poolR} = \floor{\var{appPerf}\cdot\var{maxP}} \\
      &amp; ~~~~~~~\var{mRewards} = \\
      &amp; ~~~~~~~~~~\left\{
                    \addrRw~hk\mapsto\fun{r_{member}}~ \var{poolR}~
\var{pool}~ \var{\frac{c}{tot}}~ {\sigma}
                    ~\Big\vert~
                    hk\mapsto c\in\var{stake},~~hk
\not\in(\fun{poolOwners}~\var{pool})
                  \right\}\\
      &amp; ~~~~~~~\var{lReward} = \fun{r_{operator}}~ \var{poolR}~
\var{pool}~ \var{\frac{\var{ostake}}{tot}}~ {\sigma} \\
      &amp; ~~~~~~~\var{potentialRewards} =
                 \var{mRewards} \cup
                 \{(\fun{poolRAcnt}~\var{pool})\mapsto\var{lReward}\} \\
      &amp; ~~~~~~~\var{rewards} =
\var{addrs_{rew}}\restrictdom{\var{potentialRewards}} \\
  
\end{aligned}$$</span></p>
<p><em>Calculation to reward all stake pools</em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{reward} \in \PParams \to \type{BlocksMade}\to \Coin\to
\powerset{\AddrRWD}
      \to (\KeyHash \mapsto \PoolParam) \\
      &amp; ~~~\to \type{Stake}\to (\KeyHash_{stake} \mapsto
\KeyHash_{pool}) \to
      \Coin \to (\AddrRWD \mapsto \Coin)\\
      &amp; \fun{reward}
  ~ \var{pp}~ \var{blocks}~ \var{R}~ \var{addrs_{rew}}~
\var{poolParams}~ \var{stake}~ \var{delegs}~ \var{total}
          = \var{rewards}\\
      &amp; ~~~\where \\
      &amp; ~~~~~~~\var{total}_a = \sum_{\_\mapsto c\in \var{stake}}c \\
      &amp; ~~~~~~~\var{\overline{N}} = \sum_{\_\mapsto m\in blocks}m \\
      &amp; ~~~~~~~pdata = \left\{
        hk\mapsto \left(p,~n,~\fun{poolStake}~ \var{hk}~ \var{delegs}~
\var{stake}\right)
        \mathrel{\Bigg|}
        \begin{array}{r@{\mapsto}c@{~\in~}l}
          hk &amp; \var{p} &amp; \var{poolParams} \\
          hk &amp; \var{n} &amp; \var{blocks} \\
        \end{array}
      \right\} \\
      &amp; ~~~~~~~\var{results} = \\
      &amp; ~~~~~~~\left\{
        hk \mapsto \fun{rewardOnePool}~
                     \var{pp}~
                     \var{R}~
                     \var{n}~
                     \var{\overline{N}}~
                     \var{p}~
                     \var{s}~
                     \frac{\sum s}{total}~
                     \frac{\sum s}{\var{total}_a}~
                     \var{total}~
                     \var{addrs_{rew}}
                 \mid
        hk\mapsto(p, n, s)\in\var{pdata} \right\} \\
      &amp; ~~~~~~~\var{rewards} =
\bigcup_{\wcard\mapsto\var{r}\in\var{results}}\var{r}
  
\end{aligned}$$</span></p>
<figcaption>The Reward Calculation</figcaption>
</figure>

## Reward Update Calculation {#sec:reward-calc}

This section defines the calculation of a reward update. A reward update
is the information needed to account for the movement of lovelace in the
system due to paying out rewards.

Figure [16](#fig:fund-preservation){reference-type="ref"
reference="fig:fund-preservation"} captures the potential movement of
funds in the entire system, taking every transition system in this
document into account. Value is moved between accounting pots, but the
total amount of value in the system remains constant. In particular, the
red subgraph represents the inputs and outputs to the "reward pot", a
temporary variable used during the reward update calculation in
Figure [18](#fig:functions:reward-update-creation){reference-type="ref"
reference="fig:functions:reward-update-creation"}. The blue arrows
represent the movement of funds that pass through the "reward pot".

<figure id="fig:fund-preservation">
<div class="center">

</div>
<figcaption>Preservation of Value</figcaption>
</figure>

Figure [17](#fig:defs:reward-update){reference-type="ref"
reference="fig:defs:reward-update"} defines a reward update. It consists
of four pots:

- The change to the treasury. This will be a positive value.

- The change to the reserves. This will be a negative value.

- The map of new individual rewards (to be added to the existing
  rewards).

- The change to the fee pot. This will be a negative value. rewards.

<figure id="fig:defs:reward-update">
<p><em>Reward Update</em> <span
class="math display">$$\type{RewardUpdate}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \Delta t &amp; \Coin &amp; \text{change to the treasury} \\
        \Delta r &amp; \Coin &amp; \text{change to the reserves} \\
        \var{rs} &amp; \AddrRWD\mapsto\Coin &amp; \text{new individual
rewards} \\
        \Delta f &amp; \Coin &amp; \text{change to the fee pot} \\
      \end{array}
    \right)$$</span></p>
<figcaption>Rewards Update type</figcaption>
</figure>

Figure [18](#fig:functions:reward-update-creation){reference-type="ref"
reference="fig:functions:reward-update-creation"} defines two functions,
$\fun{createRUpd}$ to create a reward update and $\fun{applyRUpd}$ to
apply a reward update to an instance of $\type{EpochState}$.

The $\fun{createRUpd}$ function does the following:

- Note that for all the calculations below, we use the previous protocol
  parameters $\var{prevPp}$, which corresponds to the parameters during
  the epoch for which we are creating rewards.

- First we calculate the change to the reserves, as determined by the
  $\rho$ protocol parameter.

- Next we calculate $\var{rewardPot}$, the total amount of coin
  available for rewards this epoch, as described in section 6.4 of
  [@delegation_design]. It consists of:

  - The fee pot, containing the transaction fees from the epoch.

  - The amount of monetary expansion from the reserves, calculated
    above.

  Note that the fee pot is taken from the snapshot taken at the epoch
  boundary. (See Figure[6](#fig:rules:snapshot){reference-type="ref"
  reference="fig:rules:snapshot"}).

- Next we calculate the proportion of the reward pot that will move to
  the treasury, as determined by the $\tau$ protocol parameter. The
  remaining pot is called the $\var{R}$, just as in section 6.5 of
  [@delegation_design].

- The rewards are calculated, using the oldest stake distribution
  snapshot (the one labeled "go"). As given by $\fun{maxPool}$, each
  pool can receive a maximal amount, determined by its performance. The
  difference between the maximal amount and the actual amount received
  is added to the amount moved to the reserves.

- The fee pot will be reduced by $\var{feeSS}$.

Note that fees are not explicitly removed from any account: the fees
come from transactions paying them and are accounted for whenever
transactions are processed.

The $\fun{applyRUpd}$ function does the following:

- Adjust the treasury, reserves and fee pots by the appropriate amounts.

- Add each individual reward to the global reward mapping. We must be
  careful, though, not to give out rewards to accounts that have been
  deregistered after the reward update was created.

  - Rewards for accounts that are still registered are added to the
    reward mappings.

  - The sum of the unregistered rewards are added to the treasury.

These two functions will be used in the blockchain transition systems in
Section [\[sec:chain\]](#sec:chain){reference-type="ref"
reference="sec:chain"}. In particular, $\fun{createRUpd}$ will be used
in
Equation [\[eq:reward-update\]](#eq:reward-update){reference-type="ref"
reference="eq:reward-update"}, and $\fun{applyRUpd}$ will be used in
Equation [\[eq:new-epoch\]](#eq:new-epoch){reference-type="ref"
reference="eq:new-epoch"}.

<figure id="fig:functions:reward-update-creation">
<p><em>Calculation to create a reward update</em> <span
class="math display">$$\begin{aligned}
    &amp; \fun{createRUpd} \in \N \to \type{BlocksMade}\to
\type{EpochState}\to \Coin \to \type{RewardUpdate}\\
    &amp;
\fun{createRUpd}~\var{slotsPerEpoch}~\var{b}~\var{es}~\var{total} =
\left(
      \Delta t_1,-~\Delta r_1+\Delta r_2,~\var{rs},~-\var{feeSS}\right)
\\
    &amp; ~~~\where \\
    &amp; ~~~~~~~(\var{acnt},~\var{ss},~\var{ls},~\var{prevPp},~\wcard)
= \var{es} \\
    &amp; ~~~~~~~(\wcard,~\wcard,~\var{pstake_{go}},~\var{feeSS}) =
\var{ss}\\
    &amp; ~~~~~~~(\var{stake},~\var{delegs},~\var{poolParams}) =
\var{pstate_{go}} \\
    &amp; ~~~~~~~(\wcard,~\var{reserves}) = \var{acnt} \\
    &amp; ~~~~~~~\left(
      \wcard,~
      \left(
      \left(\var{rewards},~\wcard,~\wcard,~\wcard,~\wcard,~\wcard\right),~
      \wcard
      \right)
      \right) = \var{ls} \\
    &amp; ~~~~~~~\Delta r_1 = \floor*{\min(1,\eta) \cdot
(\fun{rho}~\var{prevPp}) \cdot
      \var{reserves}}
    \\
    &amp; ~~~~~~~\eta =
      \begin{cases}
        1 &amp; (\fun{d}~\var{prevPp})\geq 0.8 \\
        \frac{blocksMade}{\floor{(1-\fun{d}~\var{prevPp})\cdot\var{slotsPerEpoch}
\cdot \ActiveSlotCoeff}}
          &amp; \text{otherwise} \\
      \end{cases} \\
    &amp; ~~~~~~~\var{rewardPot} = \var{feeSS} + \Delta r_1 \\
    &amp; ~~~~~~~\Delta t_1 = \floor*{(\fun{tau}~\var{prevPp}) \cdot
\var{rewardPot}} \\
    &amp; ~~~~~~~\var{R} = \var{rewardPot} - \Delta t_1 \\
    &amp; ~~~~~~~\var{circulation} = \var{total} - \var{reserves} \\
    &amp; ~~~~~~~\var{rs}
      = \fun{reward}
  ~ \var{prevPp}~ \var{b}~ \var{R}~ \var{(\dom{rewards})}~
\var{poolParams}~ \var{stake}~ \var{delegs}~ \var{circulation} \\
    &amp; ~~~~~~~\Delta r_{2} = R - \left(\sum\limits_{\_\mapsto
c\in\var{rs}}c\right) \\
    &amp; ~~~~~~~blocksMade = \sum_{\wcard \mapsto m \in b}m
  
\end{aligned}$$</span></p>
<figcaption>Reward Update Creation</figcaption>
</figure>

<figure id="fig:functions:reward-update-application">
<p><em>Applying a reward update</em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{applyRUpd} \in \type{RewardUpdate}\to
\type{EpochState}\to \type{EpochState}\\
      &amp; \fun{applyRUpd}~
      \left(
        \begin{array}{c}
          \Delta t \\
          \Delta r \\
          \var{rs} \\
          \Delta f \\
        \end{array}
    \right)
      \left(
        \begin{array}{c}
          \var{treasury} \\
          \var{reserves} \\
          ~ \\
          \var{rewards} \\
          \var{delegations} \\
          \var{ptrs} \\
          \var{genDelegs} \\
          \var{fGenDelegs} \\
          \var{i_{rwd}}
          \\~ \\
          \var{poolParams} \\
          \var{fPoolParams} \\
          \var{retiring} \\
          ~ \\
          \var{utxo} \\
          \var{deposited} \\
          \var{fees} \\
          \var{up} \\
          ~ \\
          \var{prevPp} \\
          \var{pp} \\
        \end{array}
      \right)
      =
      \left(
        \begin{array}{c}
          \varUpdate{\var{treasury} + \Delta t + \var{unregRU'}}\\
          \varUpdate{\var{reserves} + \Delta r}\\
          ~ \\
          \varUpdate{\var{rewards}\unionoverridePlus\var{regRU}} \\
          \var{delegations} \\
          \var{ptrs} \\
          \var{genDelegs} \\
          \var{fGenDelegs} \\
          \var{i_{rwd}}
          \\~ \\
          \var{poolParams} \\
          \var{fPoolParams} \\
          \var{retiring} \\
          ~ \\
          \var{utxo} \\
          \var{deposited} \\
          \varUpdate{\var{fees}+\Delta f} \\
          \var{up} \\
          ~ \\
          \var{prevPp} \\
          \var{pp} \\
        \end{array}
    \right) \\
    &amp; ~~~\where \\
    &amp; ~~~~~~~\var{regRU}=(\dom{rewards})\restrictdom rs\\
    &amp; ~~~~~~~\var{unregRU}=(\dom{rewards})\subtractdom rs\\
    &amp; ~~~~~~~\var{unregRU'}=\sum\limits_{\wcard\mapsto
c\in\var{unregRU}} \var{c}\\
  
\end{aligned}$$</span></p>
<figcaption>Reward Update Application</figcaption>
</figure>
