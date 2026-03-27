## Helper Functions and Accounting Fields {#sec:stake-dist-helpers}

Figure [1](#fig:funcs:epoch-helper-rewards){reference-type="ref"
reference="fig:funcs:epoch-helper-rewards"} defines four helper
functions needed throughout the rest of the section.

- The function $\fun{obligation}$ calculates the the minimal amount of
  coin needed to pay out all deposit refunds.

- The function $\fun{poolStake}$ filters the stake distribution to one
  stake pool.

<figure id="fig:funcs:epoch-helper-rewards">
<p><em>Total possible refunds</em> <span
class="math display">$$\begin{aligned}
    &amp; \fun{obligation} \in \PParams \to (\StakeCredential \mapsto
\Coin)
    \to (\KeyHash_{pool}\mapsto\PoolParam) \to \Coin \\
    &amp; \fun{obligation}~ \var{pp}~ \var{rewards}~ \var{poolParams} =
\\
    &amp; ~~~~~
    (\fun{keyDeposit}~\var{pp}) \cdot|\var{rewards}| +
    (\fun{poolDeposit}~\var{pp}) \cdot|\var{poolParams}| \\
  
\end{aligned}$$</span> <em>Filter Stake to one Pool</em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{poolStake} \in \KeyHash_{pool} \to (\KeyHash_{stake}
\mapsto \KeyHash_{pool})
        \to \type{Stake}\to \type{Stake}\\
      &amp; \fun{poolStake}~ \var{hk}~ \var{delegs}~ \var{stake} =
        \dom{(\var{delegs}\restrictrange\{hk\})\restrictdom\var{stake}}
  
\end{aligned}$$</span></p>
<figcaption>Helper Functions used in Rewards and Epoch
Boundary</figcaption>
</figure>

The Figure [2](#fig:defs:accounting){reference-type="ref"
reference="fig:defs:accounting"} lists the accounting fields, denoted by
$\Acnt$, which will be used throughout this section. It consists of:

- The value $\var{treasury}$ tracks the amount of coin currently stored
  in the treasury. Initially there will be no way to remove these funds.

- The value $\var{reserves}$ tracks the amount of coin currently stored
  in the reserves. This pot is used to pay rewards.

More will be said about the general accounting system in
Section [1.10](#sec:reward-calc){reference-type="ref"
reference="sec:reward-calc"}.

<figure id="fig:defs:accounting">
<p><em>Accounting Fields</em> <span class="math display">$$\Acnt =
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{treasury} &amp; \Coin &amp; \text{treasury pot}\\
        \var{reserves} &amp; \Coin &amp; \text{reserve pot}\\
      \end{array}
    \right)$$</span></p>
<figcaption>Accounting fields</figcaption>
</figure>

## Stake Distribution Calculation {#sec:stake-dist-calc}

This section defines the stake distribution calculations.
Figure [3](#fig:epoch-defs){reference-type="ref"
reference="fig:epoch-defs"} introduces three new derived types:

- $\type{BlocksMade}$ represents the number of blocks each stake pool
  produced during an epoch.

- $\type{Stake}$ represents the amount of stake (in $\type{Coin}$)
  controlled by each stake pool.

<figure id="fig:epoch-defs">
<p><em>Derived types</em> <span
class="math display">$$\begin{array}{r@{~\in~}l@{\qquad=\qquad}lr}
      \var{blocks}
      &amp; \type{BlocksMade}
      &amp; \KeyHash_{pool} \mapsto \N
      &amp; \text{blocks made by stake pools} \\
      \var{stake}
      &amp; \type{Stake}
      &amp; \Credential \mapsto \Coin
      &amp; \text{stake} \\
    \end{array}$$</span></p>
<figcaption>Epoch definitions</figcaption>
</figure>

The stake distribution calculation is given in
Figure [4](#fig:functions:stake-distribution){reference-type="ref"
reference="fig:functions:stake-distribution"}.

- $\fun{aggregate_{+}}$ takes a relation on $A\times B$, where $B$ is
  any monoid $(B,+,e)$ and returns a map from each $a\in A$ to the "sum"
  (using the monoidal $+$ operation) of all $b\in B$ such that
  $(a, b)\in A\times B$.

- $\fun{stakeDistr}$ uses the $\fun{aggregate_{+}}$ function and several
  relations to compute the stake distribution, mapping each hashkey to
  the total coin under its control. Keys that are not both registered
  and delegated are filtered out. The relation passed to
  $\fun{aggregate_{+}}$ is made up of:

  - $\fun{stakeCred_b}^{-1}$, relating credentials to (base) addresses

  - $\left(\fun{addrPtr}\circ\var{ptr}\right)^{-1}$, relating
    credentials to (pointer) addresses

  - $\range{utxo}$, relating addresses to coins

  - $\fun{stakeCred_r}^{-1}\circ\var{rewards}$, relating (reward)
    addresses to coins

  The notation for relations is explained in
  Section [\[sec:notation-shelley\]](#sec:notation-shelley){reference-type="ref"
  reference="sec:notation-shelley"}.

<figure id="fig:functions:stake-distribution">
<p><em>Aggregation (for a monoid B)</em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{aggregate_{+}} \in \powerset{(A \times B)} \to
(A\mapsto B) \\
      &amp; \fun{aggregate_{+}}~\var{R} = \left\{a\mapsto
\sum_{(a,b)\in\var{R}}b
          ~\mid~a\in\dom\var{R}\right\} \\
  
\end{aligned}$$</span> <em>Stake Distribution (using functions and maps
as relations)</em> <span class="math display">$$\begin{aligned}
      &amp; \fun{stakeDistr} \in \UTxO \to \DState \to \PState \to
\type{Snapshot}\\
      &amp; \fun{stakeDistr}~{utxo}~{dstate}~{pstate} = \\
      &amp; ~~~~ \big((\dom{\var{activeDelegs}})
      \restrictdom\left(\fun{aggregate_{+}}~\var{stakeRelation}\right),
    ~\var{delegations},~\var{poolParams}\big)\\
      &amp; \where \\
      &amp; ~~~~
(~\var{rewards},~\var{delegations},~\var{ptrs},~\wcard,~\wcard,~\wcard)
        = \var{dstate} \\
      &amp; ~~~~ (~\var{poolParams},~\wcard,~\wcard) = \var{pstate} \\
      &amp; ~~~~ \var{stakeRelation} = \left(
        \left(\fun{stakeCred_b}^{-1}\cup\left(\fun{addrPtr}\circ\var{ptr}\right)^{-1}\right)
        \circ\left(\range{\var{utxo}}\right)
        \right)
        \cup \var{rewards} \\
      &amp; ~~~~ \var{activeDelegs} =
               (\dom{rewards}) \restrictdom \var{delegations}
\restrictrange (\dom{poolParams}) \\
  
\end{aligned}$$</span></p>
<figcaption>Stake Distribution Function</figcaption>
</figure>

## Snapshot Transition {#sec:snapshots}

The state transition types for stake distribution snapshots are given in
Figure [5](#fig:ts-types:snapshot){reference-type="ref"
reference="fig:ts-types:snapshot"}. Each snapshot consists of:

- $\var{stake}$, a stake distribution, which is defined in
  Figure [3](#fig:epoch-defs){reference-type="ref"
  reference="fig:epoch-defs"} as a mapping of credentials to coin.

- $\var{delegations}$, a delegation map, mapping credentials to stake
  pools.

- $\var{poolParameters}$, storing the pool parameters of each stake
  pool.

The type $\type{\type{Snapshots}}$ contains the information needing to
be saved on the epoch boundary:

- $\var{pstake_{mark}}$, $\var{pstake_{set}}$ and $\var{pstake_{go}}$
  are the three snapshots as explained in
  Section [1.1](#sec:reward-overview){reference-type="ref"
  reference="sec:reward-overview"}.

- $\var{feeSS}$ stores the fees which are added to the reward pot during
  the next reward update calculation, which is then subtracted from the
  fee pot on the epoch boundary.

<figure id="fig:ts-types:snapshot">
<p><em>Snapshots</em> <span class="math display">$$\type{Snapshot}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{stake} &amp; \type{Stake}&amp; \text{stake distribution}\\
        \var{delegations} &amp; \Credential\mapsto\KeyHash_{pool}
                          &amp; \text{stake delegations}\\
        \var{poolParameters} &amp; \KeyHash_{pool} \mapsto \PoolParam
&amp; \text{pool parameters }\\
      \end{array}
    \right)$$</span></p>
<p><span class="math display">$$\type{Snapshots}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{pstake_{mark}} &amp; \type{Snapshot}&amp; \text{newest
stake}\\
        \var{pstake_{set}}  &amp; \type{Snapshot}&amp; \text{middle
stake}\\
        \var{pstake_{go}}   &amp; \type{Snapshot}&amp; \text{oldest
stake}\\
        \var{feeSS} &amp; \Coin &amp; \text{fee snapshot}\\
      \end{array}
    \right)$$</span> <em>Snapshot transitions</em> <span
class="math display">$$\_ \vdash
    \var{\_} \trans{snap}{} \var{\_}
    \subseteq \powerset (\LState \times \type{Snapshots}\times
\type{Snapshots})$$</span></p>
<figcaption>Snapshot transition-system types</figcaption>
</figure>

The snapshot transition rule is given in
Figure [6](#fig:rules:snapshot){reference-type="ref"
reference="fig:rules:snapshot"}. This transition has no preconditions
and results in the following state change:

- The oldest snapshot is replaced with the penultimate one.

- The penultimate snapshot is replaced with the newest one.

- The newest snapshot is replaced with one just calculated.

- The current fees pot is stored in $\var{feeSS}$. Note that this value
  will not change during the epoch, unlike the $\var{fees}$ value in the
  UTxO state.

<figure id="fig:rules:snapshot">
<p><span class="math display">$$\label{eq:snapshot}
    \inference[Snapshot]
    {
      {
      \begin{array}{r@{~\leteq~}l}
        ((\var{utxo},~\wcard,\var{fees},~\wcard),~(\var{dstate},~\var{pstate}))
&amp; \var{lstate} \\
        \var{stake} &amp; \fun{stakeDistr}~ \var{utxo}~ \var{dstate}~
\var{pstate} \\
      \end{array}
      }
    }
    {
      \begin{array}{r}
        \var{lstate} \\
      \end{array}
      \vdash
      \left(
        \begin{array}{r}
          \var{pstake_{mark}}\\
          \var{pstake_{set}}\\
          \var{pstake_{go}}\\
          \var{feeSS} \\
        \end{array}
      \right)
      \trans{snap}{}
      \left(
        \begin{array}{r}
          \varUpdate{\var{stake}} \\
          \varUpdate{\var{pstake_{mark}}} \\
          \varUpdate{\var{pstake_{set}}} \\
          \varUpdate{\var{fees}} \\
        \end{array}
      \right)
    }$$</span></p>
<figcaption>Snapshot Inference Rule</figcaption>
</figure>
