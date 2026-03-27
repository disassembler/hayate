## Protocol Parameters Update Transition {#sec:pparam-update}

Finally, reaching the epoch boundary may trigger a change in the
protocol parameters. The protocol parameters environment consists of the
delegation and pool states, and the signal is an optional new collection
of protocol parameters The state change is a change of the $\UTxOState$,
the $\Acnt$ states and the current $\PParams$. The type of this state
transition is given in
Figure [9](#fig:ts-types:new-proto-param){reference-type="ref"
reference="fig:ts-types:new-proto-param"}.

<figure id="fig:ts-types:new-proto-param">
<p><em>New Proto Param environment</em> <span
class="math display">$$\type{NewPParamEnv}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{dstate} &amp; \DState &amp; \text{delegation state}\\
        \var{pstate} &amp; \PState &amp; \text{pool state}\\
      \end{array}
    \right)$$</span> <em>New Proto Param States</em> <span
class="math display">$$\type{NewPParamState}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{utxoSt} &amp; \UTxOState &amp; \text{utxo state}\\
        \var{acnt} &amp; \Acnt &amp; \text{accounting}\\
        \var{pp} &amp; \PParams &amp; \text{current protocol
parameters}\\
      \end{array}
    \right)$$</span> <em>New Proto Param transitions</em> <span
class="math display">$$\_ \vdash
    \var{\_} \trans{newpp}{\_} \var{\_}
    \subseteq \powerset (\type{NewPParamEnv}\times
\type{NewPParamState}\times \PParams^? \times
\type{NewPParamState})$$</span></p>
<p><em>Helper Functions</em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{updatePpup} \in \UTxOState \to \PParams \to
\UTxOState\\
      &amp; \fun{updatePpup}~\var{utxoSt}~\var{pp} =
      \begin{cases}
        (\var{utxo},\var{deposited},\var{fees},(\var{fpup},~\emptyset))
        &amp;
        \var{canFollow}
        \\
        (\var{utxo},\var{deposited},\var{fees},(\emptyset,~\emptyset))
        &amp;
        \text{otherwise} \\
      \end{cases}\\
      &amp; ~~~\where \\
      &amp; ~~~~~~~\var{canFollow} =
        \forall\var{ps}\in\range{pup},~
        \var{pv}\mapsto\var{v}\in\var{ps}\implies\fun{pvCanFollow}~(\fun{pv}~\var{pp})~\var{v}
        \\
      &amp;
~~~~~~~(\var{utxo},\var{deposited},\var{fees},(\var{pup},~\var{fpup})) =
\var{utxoSt} \\
  
\end{aligned}$$</span></p>
<figcaption>New Proto Param transition-system types</figcaption>
</figure>

Figure [10](#fig:rules:new-proto-param){reference-type="ref"
reference="fig:rules:new-proto-param"} defines the new protocol
parameter transition. The transition has two rules, depending on whether
or not the new protocol parameters meet some requirements. In
particular, we require that the new parameters would not incur a debt of
the system that can not be covered by the reserves, and that the max
block size is greater than the sum of the max transaction size and the
max header size. If the requirements are met, the new protocol
parameters are accepted, the proposal is reset, and the reserves are
adjusted to account for changes in the deposits. Otherwise, the only
change is that the proposal is reset.

The $\mathsf{NEWPP}$ rule also cleans up the protocol parameter update
proposals, by calling $\fun{updatePpup}$ on the UTxO state. The
$\fun{updatePpup}$ sets the protocol parameter updates to the future
protocol parameter updates provided the protocol versions all can follow
from the version given in the protocol parameters, or the emptyset
otherwise. In any case, the future protocol parameters update proposals
are set to the empty set. If new protocol parameters are being adopted,
then these is the value given to $\fun{updatePpup}$, otherwise the old
parameters are given.

Regarding adjusting the reserves for changes in the deposits, one of
three things happens:

- If the new protocol parameters mean that **fewer** funds are required
  in the deposit pot to cover all possible refunds, then the excess is
  moved to the reserves.

- If the new protocol parameters mean that **more** funds are required
  in the deposit pot to cover all possible refunds and the difference is
  **less** than the reserve pot, then funds are moved from the reserve
  pot to cover the difference.

- If the new protocol parameters mean that **more** funds are required
  in the deposit pot to cover all possible refunds and the difference is
  **more** than the reserve pot, then
  Rule [\[eq:new-pc-denied\]](#eq:new-pc-denied){reference-type="ref"
  reference="eq:new-pc-denied"} meets the precondition and the only
  change of state is that the update proposals are reset.

Note that here, unlike most of the inference rules in this document, the
$\var{utxoSt'}$ and the $\var{acnt'}$ do not come from valid UTxO or
accounts transitions in the antecedent. We simply define the consequent
transition using these directly (instead of listing all the fields in
both states in the consequent transition). It is done this way here for
ease of reading.

<figure id="fig:rules:new-proto-param">
<p><span class="math display">$$\label{eq:new-pc-accepted}
    \hspace{-0.3cm}
    \inference[New-Proto-Param-Accept]
    {
      \var{pp_{new}}\neq\Nothing \\~\\
      {\begin{array}{rcl}
         (\var{utxo},~\var{deposited},~\var{fees},~\var{ppup}) &amp;
\leteq &amp; \var{utxoSt} \\
         \var{(\var{rewards},~\wcard,~\wcard,~\wcard,~\wcard,~\var{i_{rwd}})}
&amp;
         \leteq &amp; \var{dstate}\\
         \var{(\var{poolParams},~\wcard,~\wcard)} &amp; \leteq &amp;
\var{pstate}\\
         \var{oblg_{cur}} &amp; \leteq &amp; \fun{obligation}~ \var{pp}~
\var{rewards}~ \var{poolParams} \\
         \var{oblg_{new}} &amp; \leteq &amp; \fun{obligation}~
\var{pp_{new}}~ \var{rewards}~ \var{poolParams} \\
         \var{diff} &amp; \leteq &amp; \var{oblg_{cur}} -
\var{oblg_{new}}\\
      \end{array}}
      \\~\\~\\
      \var{oblg_{cur}} = \var{deposited} \\
      \var{reserves} + \var{diff} \geq
\sum\limits_{\wcard\mapsto\var{val}\in\var{i_{rwd}}} val \\
      \fun{maxTxSize}~\var{pp_{new}} +
\fun{maxHeaderSize}~\var{pp_{new}} &lt;
        \fun{maxBlockSize}~\var{pp_{new}}
      \\~\\
        \var{utxoSt'} \leteq
        \left(\var{utxo},~\varUpdate{oblg_{new}},~\var{fees},~\var{ppup}\right)
      \\
      \var{utxoSt''} \leteq
\fun{updatePpup}~\var{utxoSt'}~\var{pp_{new}}
      \\~\\
      (\var{treasury},~\var{reserves})\leteq \var{acnt} \\
      \var{acnt'} \leteq (\var{treasury},~\varUpdate{reserves + diff})
\\
    }
    {
      \begin{array}{l}
        \var{dstate}\\
        \var{pstate}\\
      \end{array}
      \vdash
      \left(
        \begin{array}{r}
          \var{utxoSt} \\
          \var{acnt} \\
          \var{pp}
        \end{array}
      \right)
      \trans{newpp}{\var{pp_{new}}}
      \left(
        \begin{array}{rcl}
          \varUpdate{utxoSt''}\\
          \varUpdate{acnt'} \\
          \varUpdate{\var{pp_{new}}} \\
        \end{array}
      \right)
    }$$</span></p>
<p><span class="math display">$$\label{eq:new-pc-denied}
    \inference[New-Proto-Param-Denied]
    {
      \left({\begin{array}{c}
            \var{pp_{new}}=\Nothing \\
        \lor \\
        \var{reserves} + \var{diff} &lt;
\sum\limits_{\wcard\mapsto\var{val}\in\var{i_{rwd}}} val\\
        \lor \\
        \fun{maxTxSize}~\var{pp_{new}} +
\fun{maxHeaderSize}~\var{pp_{new}} \geq
          \fun{maxBlockSize}~\var{pp_{new}}
      \end{array}}\right)
      \\~\\~\\
      {\begin{array}{rcl}
          \var{(\var{rewards},~\wcard,~\wcard,~\wcard,~\wcard,~\var{i_{rwd}})}
&amp;
          \leteq &amp; \var{dstate}\\
         \var{(\var{poolParams},~\wcard,~\wcard)} &amp; \leteq &amp;
\var{pstate}\\
          \var{oblg_{cur}} &amp; \leteq &amp; \fun{obligation}~
\var{pp}~ \var{rewards}~ \var{poolParams} \\
          \var{oblg_{new}} &amp; \leteq &amp; \fun{obligation}~
\var{pp_{new}}~ \var{rewards}~ \var{poolParams} \\
         \var{diff} &amp; \leteq &amp; \var{oblg_{cur}} -
\var{oblg_{new}}
      \end{array}}
      \\~\\~\\
      \var{utxoSt'} \leteq \fun{updatePpup}~\var{utxoSt}~\var{pp} \\
    }
    {
      \begin{array}{l}
        \var{dstate}\\
        \var{pstate}\\
      \end{array}
      \vdash
      \left(
        \begin{array}{r}
          \var{utxoSt} \\
          \var{acnt} \\
          \var{pp}
        \end{array}
      \right)
      \trans{newpp}{\var{pp_{new}}}
      \left(
        \begin{array}{rcl}
          \varUpdate{utxoSt'}\\
          \var{acnt} \\
          \var{pp}
        \end{array}
      \right)
    }$$</span></p>
<figcaption>New Proto Param Inference Rule</figcaption>
</figure>

## Complete Epoch Boundary Transition {#sec:total-epoch}

Finally, it is possible to define the complete epoch boundary transition
type, which is defined in
Figure [11](#fig:ts-types:epoch){reference-type="ref"
reference="fig:ts-types:epoch"}. The transition has no evironment. The
state is made up of the the accounting state, the snapshots, the ledger
state and the protocol parameters. The transition uses a helper function
$\fun{votedValue}$ which returns the consensus value of update proposals
in the event that consensus is met. **Note that** $\fun{votedValue}$
**is only well-defined if** $\var{quorum}$ **is greater than half the
number of core nodes, i.e.** $\Quorum > |\var{genDelegs}|/2$ **.**

<figure id="fig:ts-types:epoch">
<p><em>Epoch States</em> <span class="math display">$$\type{EpochState}=
    \left(
      \begin{array}{r@{~\in~}ll}
        \var{acnt} &amp; \Acnt &amp; \text{accounting}\\
        \var{ss} &amp; \type{Snapshots}&amp; \text{snapshots}\\
        \var{ls} &amp; \LState &amp; \text{ledger state}\\
        \var{prevPp} &amp; \PParams &amp; \text{previous protocol
parameters}\\
        \var{pp} &amp; \PParams &amp; \text{protocol parameters}\\
      \end{array}
    \right)$$</span> <em>Epoch transitions</em> <span
class="math display">$$\vdash
    \var{\_} \trans{epoch}{\_} \var{\_}
    \subseteq \powerset (\type{EpochState}\times \Epoch \times
\type{EpochState})$$</span> <em>Accessor Functions</em> <span
class="math display">$$\begin{array}{r@{~\in~}lr}
      \fun{getIR} &amp; \type{EpochState}\to (\StakeCredential \mapsto
\Coin)
                  &amp; \text{get instantaneous rewards} \\
    \end{array}$$</span> <em>Helper Functions</em> <span
class="math display">$$\begin{aligned}
      &amp; \fun{votedValue} \in (\KeyHashGen\mapsto\PParamsUpdate) \to
\PParams \to \N \to \PParamsUpdate^?\\
      &amp; \fun{votedValue}~\var{pup}~\var{pp}~\var{quorum} =
      \begin{cases}
        \var{pp}\unionoverrideRight\var{p}
          &amp; \exists! p\in\range{pup}~(|pup\restrictrange p|\geq
\var{quorum}) \\
        \Nothing &amp; \text{otherwise} \\
      \end{cases}
  
\end{aligned}$$</span></p>
<figcaption>Epoch transition-system types</figcaption>
</figure>

The epoch transition rule calls $\mathsf{SNAP}$, $\mathsf{POOLREAP}$ and
$\mathsf{NEWPP}$ in sequence. It also stores the previous protocol
parameters in $\var{prevPp}$. The previous protocol parameters will be
used for the reward calculation in the upcoming epoch, note that they
correspond to the epoch for which the rewards are being calculated.
Additionally, this transition also adopts the pool parameters
$\var{fPoolParams}$ corresponding to the pool re-registration
certificates which we submitted late in the ending epoch. The ordering
of these rules is important. The stake pools which will be updated by
$\var{fPoolParams}$ or reaped during the $\mathsf{POOLREAP}$ transition
must still be a part of the new snapshot, and so $\mathsf{SNAP}$ must
occur before these two actions. Moreover, $\mathsf{SNAP}$ sets the
deposit pot equal to current obligation, which is a property that is
preserved by $\mathsf{POOLREAP}$ and which is necessary for the
preservation of Ada property in the $\mathsf{NEWPP}$ transition.

<figure id="fig:rules:epoch">
<p><span class="math display">$$\label{eq:epoch}
    \inference[Epoch]
    {
      {
        \begin{array}{r}
          \var{lstate} \\
        \end{array}
      }
      \vdash
      { \var{ss} }
      \trans{\hyperref[fig:rules:snapshot]{snap}}{}
      { \var{ss'} }
      \\~\\
      (\var{utxoSt},~(\var{dstate},~\var{pstate}))\leteq\var{ls} \\
      (\var{poolParams},~\var{fPoolParams},~\var{retiring})\leteq\var{pstate}
      \\
      \var{pstate'}\leteq(\var{poolParams}\unionoverrideRight\var{fPoolParams},
      ~\emptyset,~\var{retiring})
      \\~\\~\\
      \var{pp}
      \vdash
      \left(
        {
          \begin{array}{r}
            \var{utxoSt} \\
            \var{acnt} \\
            \var{dstate} \\
            \var{pstate'} \\
          \end{array}
        }
      \right)
      \trans{\hyperref[fig:rules:pool-reap]{poolreap}}{e}
      \left(
      {
        \begin{array}{rcl}
            \var{utxoSt'} \\
            \var{acnt'} \\
            \var{dstate'} \\
            \var{pstate''} \\
        \end{array}
      }
      \right)
      \\~\\~\\
      \var{(\wcard,~\wcard,~\wcard,~(\var{pup},\wcard))}\leteq\var{utxoSt'}\\
      \var{pp_{new}}\leteq\fun{votedValue}~\var{pup}~\var{pp}~\Quorum\\
      {
        \begin{array}{r}
          \var{dstate'}\\
          \var{pstate''}\\
        \end{array}
      }
      \vdash
      \left(
        {
          \begin{array}{r}
            \var{utxoSt'} \\
            \var{acnt'} \\
            \var{pp}\\
          \end{array}
        }
      \right)
      \trans{\hyperref[fig:rules:new-proto-param]{newpp}}{\var{pp_{new}}}
      \left(
      {
        \begin{array}{rcl}
            \var{utxoSt''} \\
            \var{acnt''} \\
            \var{pp'}\\
        \end{array}
      }
      \right)
      \\~\\~\\
      \var{ls}' \leteq (\var{utxoSt}'',~(\var{dstate}',~\var{pstate}''))
    }
    {
      \vdash
      \left(
      \begin{array}{r}
        \var{acnt} \\
        \var{ss} \\
        \var{ls} \\
        \var{prevPp} \\
        \var{pp} \\
      \end{array}
      \right)
      \trans{epoch}{e}
      \left(
      \begin{array}{rcl}
        \varUpdate{\var{acnt''}} \\
        \varUpdate{\var{ss'}} \\
        \varUpdate{\var{ls'}} \\
        \varUpdate{\var{pp}} \\
        \varUpdate{\var{pp'}} \\
      \end{array}
      \right)
    }$$</span></p>
<figcaption>Epoch Inference Rule</figcaption>
</figure>
