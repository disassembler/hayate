## Pool Reaping Transition {#sec:pool-reap}

Figure [7](#fig:ts-types:pool-reap){reference-type="ref"
reference="fig:ts-types:pool-reap"} defines the types for the pool reap
transition, which is responsible for removing pools slated for
retirement in the given epoch.

<figure id="fig:ts-types:pool-reap">
<p><em>Pool Reap State</em> <span
class="math display">$$\type{PlReapState}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{utxoSt} &amp; \UTxOState &amp; \text{utxo state}\\
        \var{acnt} &amp; \Acnt &amp; \text{accounting}\\
        \var{dstate} &amp; \DState &amp; \text{delegation state}\\
        \var{pstate} &amp; \PState &amp; \text{pool state}\\
      \end{array}
    \right)$$</span> <em>Pool Reap transitions</em> <span
class="math display">$$\_ \vdash \_ \trans{poolreap}{\_} \_ \in
    \powerset (\PParams \times \type{PlReapState}\times \Epoch \times
\type{PlReapState})$$</span></p>
<figcaption>Pool Reap Transition</figcaption>
</figure>

The pool-reap transition rule is given in
Figure [8](#fig:rules:pool-reap){reference-type="ref"
reference="fig:rules:pool-reap"}. This transition has no preconditions
and results in the following state change:

- For each retiring pool, the refund for the pool registration deposit
  is added to the pool's registered reward account, provided the reward
  account is still registered.

- The sum of all the refunds attached to unregistered reward accounts
  are added to the treasury.

- The deposit pool is reduced by the amount of claimed and unclaimed
  refunds.

- Any delegation to a retiring pool is removed.

- Each retiring pool is removed from all four maps in the pool state.

<figure id="fig:rules:pool-reap">
<p><span class="math display">$$\label{eq:pool-reap}
    \inference[Pool-Reap]
    {
      {
      \begin{array}{r@{~\leteq~}l}
        \var{retired} &amp; \dom{(\var{retiring}^{-1}~\var{e})} \\
        \var{pr} &amp; \left\{
                   \var{hk}\mapsto(\fun{poolDeposit}~\var{pp})
                     \mid
                     \var{hk}\in\var{retired}
                   \right\}\\
        \var{rewardAcnts}
                 &amp; \{\var{hk}\mapsto \fun{poolRAcnt}~\var{pool} \mid
                   \var{hk}\mapsto\var{pool} \in
\var{retired}\restrictdom\var{poolParams} \} \\
        \var{rewardAcnts'} &amp; \left\{
                               a \mapsto
                               \sum\var{pr}(\var{rewardAcnts}^{-1}(a))
                               \mathrel{\Big|}
                               a\in\range{rewardAcnts}
                             \right\} \\
        \var{refunds} &amp; \dom{rewards}\restrictdom\var{rewardAcnts'}
\\
        \var{mRefunds} &amp; \dom{rewards}\subtractdom\var{rewardAcnts'}
\\
        \var{refunded} &amp; \sum\limits_{\wcard\mapsto
c\in\var{refunds}} c \\
        \var{unclaimed} &amp; \sum\limits_{\wcard\mapsto
c\in\var{mRefunds}} c \\
      \end{array}
      }
    }
    {
      \var{pp}
      \vdash
      \left(
        \begin{array}{r}
          \var{utxo} \\
          \var{deposited} \\
          \var{fees} \\
          \var{ppup} \\
          ~ \\
          \var{treasury} \\
          \var{reserves} \\
          ~ \\
          \var{rewards} \\
          \var{delegations} \\
          \var{ptrs} \\
          \var{genDelegs} \\
          \var{fGenDelegs} \\
          \var{i_{rwd}} \\
          ~ \\
          \var{poolParams} \\
          \var{fPoolParams} \\
          \var{retiring} \\
        \end{array}
      \right)
      \trans{poolreap}{e}
      \left(
        \begin{array}{rcl}
          \var{utxo} \\
          \varUpdate{\var{deposited}}
          &amp; \varUpdate{-}
          &amp; \varUpdate{(\var{unclaimed} + \var{refunded})} \\
          \var{fees} \\
          \var{ppup} \\
          ~ \\
          \varUpdate{\var{treasury}} &amp; \varUpdate{+} &amp;
\varUpdate{\var{unclaimed}} \\
          \var{reserves} \\
          ~ \\
          \varUpdate{\var{rewards}} &amp; \varUpdate{\unionoverridePlus}
&amp; \varUpdate{\var{refunds}} \\
          \varUpdate{\var{delegations}} &amp; \varUpdate{\subtractrange}
&amp; \varUpdate{\var{retired}} \\
          \var{ptrs} \\
          \var{genDelegs} \\
          \var{fGenDelegs} \\
          \var{i_{rwd}}\\
          ~ \\
          \varUpdate{\var{retired}} &amp; \varUpdate{\subtractdom} &amp;
\varUpdate{\var{poolParams}} \\
          \varUpdate{\var{retired}} &amp; \varUpdate{\subtractdom} &amp;
\varUpdate{\var{fPoolParams}} \\
          \varUpdate{\var{retired}} &amp; \varUpdate{\subtractdom} &amp;
\varUpdate{\var{retiring}} \\
        \end{array}
      \right)
    }$$</span></p>
<figcaption>Pool Reap Inference Rule</figcaption>
</figure>
