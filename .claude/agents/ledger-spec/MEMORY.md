# Ledger Spec Agent Memory

## Converting Shelley Spec LaTeX to Markdown

The Shelley formal spec LaTeX sources are available locally at:
`/home/sam/work/iohk/cardano-ledger/master/eras/shelley/formal-spec/`

**Step 1 — Convert with pandoc:**
```bash
cd /home/sam/work/iohk/cardano-ledger/master/eras/shelley/formal-spec
nix-shell -p pandoc --run "pandoc <file>.tex -o /tmp/<file>.md"
```

**Known behaviour**: The spec uses custom iohk LaTeX macros (`\fun{}`, `\var{}`, `\PParams`,
`\StakeCredential`, etc.) defined in `iohk.sty`. Pandoc cannot resolve these and emits warnings,
but the output is still usable:
- Prose sections convert cleanly to Markdown
- Math blocks are preserved as `$$...$$` with the custom macros intact inside them
- `\fun{x}` = function x, `\var{x}` = variable x — readable once you know the convention
- ~33 warnings is normal for a full chapter file; they don't cause failure

**Step 2 — Split into topic files:**
Create a subdirectory named after the spec file (e.g. `epoch/`). Split on `##` headings and
group related sections. Use this Python snippet (via `nix-shell -p python3 --run "python3 -c '...'"``):

```python
import re
with open('/tmp/<file>.md') as f:
    content = f.read()
sections = re.split(r'(?=^## )', content, flags=re.MULTILINE)
# sections[0] = top-level heading + intro; sections[1:] = ## sections keyed by first line
rest = {s.split('\n')[0].strip(): s for s in sections[1:]}
# then write grouped sections to separate files
```

Store output files in `.claude/agents/ledger-spec/<spec-name>/`.

## Key Spec Files and What They Cover

| File | Content |
|------|---------|
| `epoch.tex` | EPOCH/NEWEPOCH STS rules, SNAP, POOLREAP, mark/set/go snapshots, reward cycle overview |
| `incentives.tex` | Reward calculation, maxPool formula, apparent performance, deltaR1/deltaR2 |
| `delegation.tex` | Stake delegation, pool registration, key/pool deposits |
| `ledger.tex` | Top-level ledger rules, LEDGER STS |
| `transactions.tex` | Transaction structure and UTxO rules |
| `update.tex` | Protocol parameter update rules (PPUP) |

## What's Been Researched

### epoch.tex → `epoch/`
Split into 5 spec files + 1 validation guide:
- `reward-overview.md` (103 lines) — reward cycle timeline (A→G), mark/set/go mnemonic
- `stake-snapshots.md` (264 lines) — helper functions, stake distribution calc, SNAP STS rule
- `pool-reaping.md` (135 lines) — POOLREAP STS rule, retirement timing
- `epoch-boundary.md` (389 lines) — PPUP transition, complete EPOCH STS rule
- `reward-calculation.md` (502 lines) — reward distribution calc, RUPD/reward update calc
- `rupd-validation-guide.md` — **START HERE for dump analysis**: JSON field→spec variable
  mapping, complete RUPD formula chain in order, epoch N→N+1 carry-forward verification,
  why rs={} causes, `rewardOnePool` quick reference, step-by-step dump pair walkthrough
