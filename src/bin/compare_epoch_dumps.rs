/// compare-epoch-dumps: Compare hayate epoch JSON dumps against Haskell cardano-node dumps.
///
/// RUPD alignment: Haskell epoch N .rupd == Hayate epoch N+1 .rupd
///   (Haskell stores the RUPD being computed for next epoch; hayate stores what was just applied)
///
/// State alignment: Haskell epoch N .{treasury,reserves,...} == Hayate epoch N .{...}
///   (Both reflect state after the epoch N→N+1 transition was applied)

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "compare-epoch-dumps")]
struct Args {
    /// Directory containing Haskell dumps (format: {epoch}-{slot}.json)
    #[arg(long)]
    haskell: PathBuf,

    /// Directory containing Hayate dumps (format: {epoch}-hayate.json)
    #[arg(long)]
    hayate: PathBuf,

    /// Start from this epoch (default: 1)
    #[arg(long, default_value_t = 1)]
    from_epoch: u64,

    /// Stop after this epoch (default: compare all)
    #[arg(long)]
    to_epoch: Option<u64>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let haskell_dumps = load_dir(&args.haskell, false)?;
    let hayate_dumps = load_dir(&args.hayate, true)?;

    println!("Loaded {} Haskell dumps, {} Hayate dumps", haskell_dumps.len(), hayate_dumps.len());

    let max_epoch = args.to_epoch.unwrap_or_else(|| {
        *haskell_dumps.keys().max().unwrap_or(&0)
    });

    let mut all_ok = true;

    for epoch in args.from_epoch..=max_epoch {
        let haskell = match haskell_dumps.get(&epoch) {
            Some(v) => v,
            None => continue,
        };
        let hayate = match hayate_dumps.get(&epoch) {
            Some(v) => v,
            None => {
                println!("  epoch {epoch}: MISSING hayate dump");
                continue;
            }
        };
        // The RUPD in Haskell[N] is computed during epoch N, applied at epoch N+1.
        // The RUPD in Hayate[N+1] is what was applied to produce epoch N+1 state.
        let hayate_next = hayate_dumps.get(&(epoch + 1));

        let mut critical: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // --- Critical: state fields (both at epoch N) ---
        compare_u64(haskell, hayate, "treasury", &mut critical);
        compare_u64(haskell, hayate, "reserves", &mut critical);
        compare_u64(haskell, hayate, "epochFees", &mut warnings);
        compare_deposits(haskell, hayate, &mut warnings);

        // --- Critical: protocol parameters (if Haskell dump has them) ---
        compare_protocol_params(haskell, hayate, &mut critical);

        // --- Critical: RUPD (Haskell[N] vs Hayate[N+1]) ---
        if let Some(hn) = hayate_next {
            compare_rupd(haskell, hn, epoch, &mut critical);
        }

        // --- Informational: snapshot contents ---
        compare_snapshots(haskell, hayate, &mut warnings);

        if critical.is_empty() && warnings.is_empty() {
            println!("epoch {epoch}: OK");
        } else if critical.is_empty() {
            println!("epoch {epoch}: OK (warnings)");
            for w in &warnings {
                println!("  WARN: {w}");
            }
        } else {
            println!("epoch {epoch}: DIVERGED");
            for m in &critical {
                println!("  CRITICAL: {m}");
            }
            for w in &warnings {
                println!("  WARN: {w}");
            }
            all_ok = false;
            break;
        }
    }

    if all_ok {
        println!("\nAll compared epochs match.");
    }

    Ok(())
}

fn load_dir(dir: &PathBuf, is_hayate: bool) -> Result<BTreeMap<u64, Value>> {
    let mut map = BTreeMap::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") {
            continue;
        }
        let epoch = if is_hayate {
            // format: {epoch}-hayate.json
            name.strip_suffix("-hayate.json")
                .and_then(|s| s.parse::<u64>().ok())
        } else {
            // format: {epoch}-{slot}.json
            name.strip_suffix(".json")
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<u64>().ok())
        };
        if let Some(epoch) = epoch {
            let content = std::fs::read_to_string(entry.path())?;
            if content.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Warning: skipping {} ({})", entry.path().display(), e);
                    continue;
                }
            };
            map.insert(epoch, value);
        }
    }
    Ok(map)
}

fn get_u64(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

fn compare_u64(haskell: &Value, hayate: &Value, key: &str, out: &mut Vec<String>) {
    let h = get_u64(haskell, key);
    let r = get_u64(hayate, key);
    match (h, r) {
        (Some(hv), Some(rv)) if hv == rv => {}
        (Some(hv), Some(rv)) => out.push(format!(
            "{key}: haskell={hv} hayate={rv} diff={} ({:.6} ADA)",
            rv as i128 - hv as i128,
            (rv as i128 - hv as i128) as f64 / 1_000_000.0
        )),
        (None, Some(_)) => out.push(format!("{key}: missing in haskell")),
        (Some(_), None) => out.push(format!("{key}: missing in hayate")),
        (None, None) => {}
    }
}

fn compare_deposits(haskell: &Value, hayate: &Value, out: &mut Vec<String>) {
    let hd = &haskell["deposits"];
    let rd = &hayate["deposits"];
    for key in &["stakeKey", "pool", "dRep", "proposal", "total"] {
        let hv = hd.get(key).and_then(|v| v.as_u64());
        let rv = rd.get(key).and_then(|v| v.as_u64());
        match (hv, rv) {
            (Some(h), Some(r)) if h == r => {}
            (Some(h), Some(r)) => out.push(format!(
                "deposits.{key}: haskell={h} hayate={r} diff={}",
                r as i128 - h as i128
            )),
            (None, Some(_)) => out.push(format!("deposits.{key}: missing in haskell")),
            (Some(_), None) => out.push(format!("deposits.{key}: missing in hayate")),
            (None, None) => {}
        }
    }
}

fn compare_protocol_params(haskell: &Value, hayate: &Value, out: &mut Vec<String>) {
    // Haskell dumps include prevPParamsEpochStateL under "protocolParams".
    // Compare the reward-critical fields if present.
    let hp = match haskell.get("protocolParams") {
        Some(v) if !v.is_null() => v,
        _ => return, // Haskell dump doesn't have this field yet
    };
    let rp = match hayate.get("protocolParams") {
        Some(v) if !v.is_null() => v,
        _ => return,
    };

    // nOpt (k) — critical for pool saturation calculation
    let h_nopt = hp.get("nOpt").and_then(|v| v.as_u64());
    let r_nopt = rp.get("nOpt").and_then(|v| v.as_u64());
    match (h_nopt, r_nopt) {
        (Some(h), Some(r)) if h != r =>
            out.push(format!("protocolParams.nOpt: haskell={h} hayate={r}")),
        _ => {}
    }

    // rho — monetary expansion rate
    let h_rho = hp.get("rho").and_then(|v| v.as_f64());
    let r_rho = rp.get("rho").and_then(|v| v.as_f64());
    match (h_rho, r_rho) {
        (Some(h), Some(r)) if (h - r).abs() > 1e-9 =>
            out.push(format!("protocolParams.rho: haskell={h} hayate={r}")),
        _ => {}
    }

    // tau — treasury growth rate
    let h_tau = hp.get("tau").and_then(|v| v.as_f64());
    let r_tau = rp.get("tau").and_then(|v| v.as_f64());
    match (h_tau, r_tau) {
        (Some(h), Some(r)) if (h - r).abs() > 1e-9 =>
            out.push(format!("protocolParams.tau: haskell={h} hayate={r}")),
        _ => {}
    }

    // a0 — pool pledge influence
    let h_a0 = hp.get("a0").and_then(|v| v.as_f64());
    let r_a0 = rp.get("a0").and_then(|v| v.as_f64());
    match (h_a0, r_a0) {
        (Some(h), Some(r)) if (h - r).abs() > 1e-9 =>
            out.push(format!("protocolParams.a0: haskell={h} hayate={r}")),
        _ => {}
    }

    // d — decentralization (may not be present post-Shelley)
    let h_d = hp.get("d").and_then(|v| v.as_f64());
    let r_d = rp.get("d").and_then(|v| v.as_f64());
    match (h_d, r_d) {
        (Some(h), Some(r)) if (h - r).abs() > 1e-9 =>
            out.push(format!("protocolParams.d: haskell={h} hayate={r}")),
        _ => {}
    }
}

fn compare_rupd(haskell: &Value, hayate_next: &Value, epoch: u64, out: &mut Vec<String>) {
    let hr = &haskell["rupd"];
    let rr = &hayate_next["rupd"];

    if hr.is_null() || hr == &Value::Null {
        out.push(format!("rupd: missing in haskell[{epoch}]"));
        return;
    }
    if rr.is_null() || rr == &Value::Null {
        out.push(format!("rupd: missing in hayate[{}]", epoch + 1));
        return;
    }

    let prefix = format!("rupd (haskell[{epoch}] vs hayate[{}])", epoch + 1);
    for key in &["deltaR1", "deltaR2", "deltaT1", "rPot", "rewardPot", "totalDistributed"] {
        let hv = hr.get(key).and_then(|v| v.as_u64());
        let rv = rr.get(key).and_then(|v| v.as_u64());
        match (hv, rv) {
            (Some(h), Some(r)) if h == r => {}
            (Some(h), Some(r)) => out.push(format!(
                "{prefix}.{key}: haskell={h} hayate={r} diff={} ({:.3} ADA)",
                r as i128 - h as i128,
                (r as i128 - h as i128) as f64 / 1_000_000.0
            )),
            (None, Some(r)) => out.push(format!("{prefix}.{key}: missing in haskell, hayate={r}")),
            (Some(h), None) => out.push(format!("{prefix}.{key}: haskell={h}, missing in hayate")),
            (None, None) => {}
        }
    }
}

fn snapshot_stake_total(snap: &Value) -> u64 {
    snap.get("stake")
        .and_then(|s| s.as_object())
        .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
        .unwrap_or(0)
}

fn snapshot_pool_stakes(snap: &Value) -> HashMap<String, u64> {
    // Haskell uses per-credential stake; pool stake is summed per pool via delegations
    // Hayate has explicit poolStake field — use that if present, else derive
    if let Some(ps) = snap.get("poolStake").and_then(|v| v.as_object()) {
        return ps.iter()
            .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0)))
            .collect();
    }
    // Haskell: derive from stake + delegations
    let stake = snap.get("stake").and_then(|v| v.as_object());
    let delegations = snap.get("delegations").and_then(|v| v.as_object());
    let mut pool_map: HashMap<String, u64> = HashMap::new();
    if let (Some(stake), Some(delegations)) = (stake, delegations) {
        for (cred, pool_val) in delegations {
            if let Some(pool_id) = pool_val.as_str() {
                // Normalize: trim to 56 hex chars (28 bytes)
                let pool_key = pool_id[..56.min(pool_id.len())].to_string();
                // Find matching stake entry — Haskell cred is 28 bytes (56 hex), hayate is 32 bytes (64 hex)
                let cred_norm = &cred[cred.len().saturating_sub(64)..]; // last 64 chars
                let cred_key = format!("keyHash-{cred_norm}");
                let amount = stake.get(&cred_key)
                    .or_else(|| stake.get(cred))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                *pool_map.entry(pool_key).or_insert(0) += amount;
            }
        }
    }
    pool_map
}

fn normalize_cred(s: &str) -> String {
    // Both "keyHash-{56hex}" and "keyHash-{64hex with trailing zeros}" should normalize
    // to the 28-byte (56 char) prefix
    let hex_part = s.strip_prefix("keyHash-").unwrap_or(s);
    format!("keyHash-{}", &hex_part[..56.min(hex_part.len())])
}

fn compare_snapshots(haskell: &Value, hayate: &Value, out: &mut Vec<String>) {
    let snaps = &[("mark", "mark"), ("set", "set"), ("go", "go")];
    for (hname, rname) in snaps {
        let hs = haskell.get("snapshots").and_then(|s| s.get(hname));
        let rs = hayate.get("snapshots").and_then(|s| s.get(rname));
        match (hs, rs) {
            (None, None) => {}
            (None, Some(_)) => out.push(format!("snapshots.{hname}: missing in haskell")),
            (Some(_), None) => out.push(format!("snapshots.{hname}: missing in hayate")),
            (Some(h), Some(r)) => {
                if h.is_null() && r.is_null() {
                    continue;
                }
                // Total stake
                let ht = snapshot_stake_total(h);
                let rt = snapshot_stake_total(r);
                if ht != rt {
                    out.push(format!(
                        "snapshots.{hname}.totalStake: haskell={ht} hayate={rt} diff={}",
                        rt as i128 - ht as i128
                    ));
                }

                // Per-credential stake (normalized to 28-byte cred key)
                let hstake: HashMap<String, u64> = h.get("stake")
                    .and_then(|s| s.as_object())
                    .map(|m| m.iter().map(|(k, v)| (normalize_cred(k), v.as_u64().unwrap_or(0))).collect())
                    .unwrap_or_default();
                let rstake: HashMap<String, u64> = r.get("stake")
                    .and_then(|s| s.as_object())
                    .map(|m| m.iter().map(|(k, v)| (normalize_cred(k), v.as_u64().unwrap_or(0))).collect())
                    .unwrap_or_default();
                for (cred, hv) in &hstake {
                    let rv = rstake.get(cred).copied().unwrap_or(0);
                    if *hv != rv {
                        out.push(format!(
                            "snapshots.{hname}.stake[{cred}]: haskell={hv} hayate={rv}"
                        ));
                    }
                }
                for (cred, rv) in &rstake {
                    if !hstake.contains_key(cred) {
                        out.push(format!(
                            "snapshots.{hname}.stake[{cred}]: missing in haskell, hayate={rv}"
                        ));
                    }
                }

                // Pool params: pledge, margin, cost
                let hparams = h.get("poolParams").and_then(|v| v.as_object());
                let rparams = r.get("poolParams").and_then(|v| v.as_object());
                if let (Some(hp), Some(rp)) = (hparams, rparams) {
                    for (pool_id, hpool) in hp {
                        let pool_key = &pool_id[..56.min(pool_id.len())];
                        if let Some(rpool) = rp.get(pool_key).or_else(|| rp.get(pool_id.as_str())) {
                            for field in &["pledge", "cost"] {
                                let hv = hpool.get(field).and_then(|v| v.as_u64());
                                let rv = rpool.get(field).and_then(|v| v.as_u64());
                                if hv != rv {
                                    out.push(format!(
                                        "snapshots.{hname}.poolParams[{pool_key}].{field}: haskell={hv:?} hayate={rv:?}"
                                    ));
                                }
                            }
                            // margin: compare as rational (multiply both by 1e9 to avoid float)
                            let hm = hpool.get("margin").and_then(|v| v.as_f64());
                            let rm = rpool.get("margin").and_then(|v| v.as_f64());
                            if let (Some(h), Some(r)) = (hm, rm) {
                                if (h - r).abs() > 1e-9 {
                                    out.push(format!(
                                        "snapshots.{hname}.poolParams[{pool_key}].margin: haskell={h} hayate={r}"
                                    ));
                                }
                            }
                        } else {
                            out.push(format!(
                                "snapshots.{hname}.poolParams[{pool_key}]: missing in hayate"
                            ));
                        }
                    }
                }
            }
        }
    }
}
