/// compare-epoch-dumps: Compare hayate epoch JSON dumps against Haskell cardano-node dumps,
/// and trace RUPD reward calculations between consecutive Haskell dumps.
///
/// RUPD alignment: Haskell epoch N .rupdNext == Hayate epoch N+1 .rupd
///   (Haskell's rupdNext is the RUPD computed during epoch N for application at N→N+1;
///    Hayate's epoch N+1 .rupd is what was just applied to produce that state)
///
/// State alignment: Haskell epoch N .{treasury,reserves,...} == Hayate epoch N .{...}
///   (Both reflect state after the epoch N→N+1 transition was applied)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "compare-epoch-dumps")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compare hayate epoch JSON dumps against Haskell cardano-node dumps
    Compare {
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

        /// Continue comparing after finding critical divergences (default: stop on first critical)
        #[arg(long)]
        keep_going: bool,
    },

    /// Trace RUPD reward calculation between two consecutive Haskell epoch dumps.
    ///
    /// Shows every formula step (η, Δr₁, feeSS, rPot, Δt₁, R, rs, Δr₂) and
    /// verifies carry-forward of treasury and reserves into epoch N+1.
    ///
    /// Example:
    ///   compare-epoch-dumps rupd-trace snap-dumps/4-345612.json snap-dumps/5-432019.json \
    ///     --expected-blocks 4320
    RupdTrace {
        /// Haskell dump for epoch N (contains rupdNext, go snapshot, protocolParams)
        epoch_n: PathBuf,

        /// Haskell dump for epoch N+1 (contains rupdApplied and post-transition reserves/treasury)
        epoch_n1: PathBuf,

        /// Expected blocks per epoch (slotsPerEpoch × activeSlotsCoeff).
        /// Preview = 4320 (86400 × 0.05), Mainnet = 21600 (432000 × 0.05).
        /// If omitted, η is shown with actual block count but not independently verified.
        #[arg(long)]
        expected_blocks: Option<u64>,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Compare { haskell, hayate, from_epoch, to_epoch, keep_going } =>
            run_compare(&haskell, &hayate, from_epoch, to_epoch, keep_going),
        Command::RupdTrace { epoch_n, epoch_n1, expected_blocks } =>
            run_rupd_trace(&epoch_n, &epoch_n1, expected_blocks),
    }
}

// ─── rupd-trace ──────────────────────────────────────────────────────────────

fn run_rupd_trace(epoch_n: &PathBuf, epoch_n1: &PathBuf, expected_blocks: Option<u64>) -> Result<()> {
    let n: Value = serde_json::from_str(
        &std::fs::read_to_string(epoch_n).with_context(|| format!("reading {}", epoch_n.display()))?
    )?;
    let n1: Value = serde_json::from_str(
        &std::fs::read_to_string(epoch_n1).with_context(|| format!("reading {}", epoch_n1.display()))?
    )?;

    let epoch_num = epoch_n.file_name()
        .and_then(|f| f.to_str())
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    println!("=== RUPD Trace: Epoch {} → {} ===\n", epoch_num, epoch_num + 1);

    // Protocol params
    let pp = &n["protocolParams"];
    let rho = get_rational_param(pp, "rho");
    let tau = get_rational_param(pp, "tau");
    let d_param = get_rational_param(pp, "d");
    let n_opt = pp.get("nOpt").and_then(|v| v.as_u64());

    // Epoch N state
    let reserves_n = n.get("reserves").and_then(|v| v.as_u64()).unwrap_or(0);
    let treasury_n = n.get("treasury").and_then(|v| v.as_u64()).unwrap_or(0);

    // Epoch N+1 state
    let reserves_n1 = n1.get("reserves").and_then(|v| v.as_u64()).unwrap_or(0);
    let treasury_n1 = n1.get("treasury").and_then(|v| v.as_u64()).unwrap_or(0);

    // rupdNext from epoch N
    let rupd = &n["rupdNext"];
    let dump_delta_r1 = rupd.get("deltaR1").and_then(|v| v.as_u64());
    let dump_delta_r2 = rupd.get("deltaR2").and_then(|v| v.as_u64());
    let dump_delta_t1 = rupd.get("deltaT1").and_then(|v| v.as_u64());
    let dump_rpot    = rupd.get("rPot").and_then(|v| v.as_u64());
    let dump_reward_pot = rupd.get("rewardPot").and_then(|v| v.as_u64());
    let dump_total_distributed = rupd.get("totalDistributed").and_then(|v| v.as_u64());
    let dump_reward_payouts = rupd.get("rewardPayouts").and_then(|v| v.as_object());

    // go snapshot blocks (field may be "blocks" or "blocksByPool")
    let go = n.pointer("/snapshots/go");
    let blocks_obj = go.and_then(|g| {
        g.get("blocks").or_else(|| g.get("blocksByPool"))
    }).and_then(|v| v.as_object());

    let total_blocks: u64 = blocks_obj
        .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
        .unwrap_or(0);

    // ── Protocol params header ──
    print!("Protocol params:");
    if let Some((n, d)) = &rho   { print!("  ρ={}/{}", n, d); }
    if let Some((n, d)) = &tau   { print!("  τ={}/{}", n, d); }
    if let Some((n, d)) = &d_param { print!("  d={}/{}", n, d); }
    if let Some(k) = n_opt       { print!("  k={}", k); }
    println!("\n");

    // ── go.blocksByPool ──
    println!("go.blocksByPool:");
    if let Some(bm) = &blocks_obj {
        let mut sorted: Vec<_> = bm.iter().collect();
        sorted.sort_by_key(|(k, _)| k.clone());
        for (pool, count) in &sorted {
            println!("  {}  {}",
                abbrev(pool, 16),
                fmt_u64(count.as_u64().unwrap_or(0)));
        }
    }
    println!("  total (n) = {}\n", fmt_u64(total_blocks));

    // ── η ──
    // Prefer --expected-blocks arg; fall back to dump's expectedBlocks field.
    let expected_blocks_resolved = expected_blocks
        .or_else(|| n.get("expectedBlocks").and_then(|v| v.as_u64()));
    let d_ge_08 = d_param.as_ref().map_or(false, |&(n, d)| n * 5 >= d * 4);
    let (eta_n, eta_d) = if d_ge_08 {
        println!("d ≥ 0.8 → η = 1  [spec: fully decentralised]");
        (1u64, 1u64)
    } else if let Some(eb) = expected_blocks_resolved {
        let (en, ed) = if total_blocks >= eb { (eb, eb) } else { (total_blocks, eb) };
        let src = if expected_blocks.is_some() { "--expected-blocks" } else { "dump" };
        println!("expectedBlocks = {} ({})", fmt_u64(eb), src);
        println!("η = {}/{} = {:.6}", total_blocks, eb, total_blocks as f64 / eb as f64);
        if total_blocks >= eb {
            println!("min(1,η) = 1  [capped]");
        } else {
            println!("min(1,η) = {}/{}", en, ed);
        }
        (en, ed)
    } else {
        println!("expectedBlocks = ? (pass --expected-blocks or ensure dump contains expectedBlocks)");
        println!("actual blocks (n) = {}", fmt_u64(total_blocks));
        (0u64, 1u64) // sentinel — skip independent Δr₁ verification
    };
    println!();

    // ── Starting balances ──
    let fee_ss = n.get("epochFees").and_then(|v| v.as_u64());
    println!("reserves_N = {}", fmt_u64(reserves_n));
    println!("treasury_N = {}", fmt_u64(treasury_n));
    match fee_ss {
        Some(f) => println!("feeSS      = {} (epochFees)", fmt_u64(f)),
        None    => println!("feeSS      = ? (epochFees not in dump)"),
    }
    println!();

    // ── Step 1: Δr₁ ──
    println!("Step 1: Δr₁ = floor(η · ρ · reserves_N)");
    if let (Some((rn, rd)), Some(eb)) = (&rho, expected_blocks) {
        let computed = floor_mul3(eta_n, eta_d, *rn, *rd, reserves_n as u128);
        let ok = dump_delta_r1.map_or(false, |d| d == computed);
        println!("  = floor({}/{} · {}/{} · {})", eta_n, eta_d, rn, rd, fmt_u64(reserves_n));
        println!("  = {}  {}", fmt_u64(computed), chk(ok));
        if let Some(d) = dump_delta_r1 { if !ok { println!("  dump: {}", fmt_u64(d)); } }
        let _ = eb;
    } else if let Some(d) = dump_delta_r1 {
        println!("  = {} (from dump; pass --expected-blocks to verify)", fmt_u64(d));
    }
    let delta_r1 = dump_delta_r1.unwrap_or(0);
    println!();

    // ── Step 2: rPot ──
    println!("Step 2: rPot = feeSS + Δr₁");
    let r_pot = dump_rpot.unwrap_or(delta_r1 + fee_ss.unwrap_or(0));
    if let Some(fee) = fee_ss {
        let computed = fee + delta_r1;
        let ok = computed == r_pot;
        println!("  = {} + {} = {}  {}", fmt_u64(fee), fmt_u64(delta_r1), fmt_u64(computed), chk(ok));
        if let Some(rp) = dump_rpot { if !ok { println!("  dump rPot: {}", fmt_u64(rp)); } }
    } else if let Some(rp) = dump_rpot {
        println!("  = {} (from dump)", fmt_u64(rp));
    }
    println!();

    // ── Step 3: Δt₁ ──
    println!("Step 3: Δt₁ = floor(τ · rPot)");
    if let Some((tn, td)) = &tau {
        let computed = ((r_pot as u128 * *tn as u128) / *td as u128) as u64;
        let ok = dump_delta_t1.map_or(false, |d| d == computed);
        println!("  = floor({}/{} · {})", tn, td, fmt_u64(r_pot));
        println!("  = {}  {}", fmt_u64(computed), chk(ok));
        if let Some(d) = dump_delta_t1 { if !ok { println!("  dump: {}", fmt_u64(d)); } }
    } else if let Some(d) = dump_delta_t1 {
        println!("  = {} (from dump)", fmt_u64(d));
    }
    let delta_t1 = dump_delta_t1.unwrap_or(0);
    println!();

    // ── Step 4: R ──
    println!("Step 4: R = rPot - Δt₁  (available for pools + delegators)");
    let r_computed = r_pot.saturating_sub(delta_t1);
    let ok4 = dump_reward_pot.map_or(true, |rp| rp == r_computed);
    println!("  = {} - {} = {}  {}", fmt_u64(r_pot), fmt_u64(delta_t1), fmt_u64(r_computed), chk(ok4));
    if let Some(rp) = dump_reward_pot { if !ok4 { println!("  dump rewardPot: {}", fmt_u64(rp)); } }
    let reward_pot = dump_reward_pot.unwrap_or(r_computed);
    println!();

    // ── Step 5: rs ──
    println!("Step 5: rs (rewardPayouts — computed at RUPD creation)");
    let sigma_rs: u64 = dump_reward_payouts
        .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
        .unwrap_or(0);
    if let Some(pm) = &dump_reward_payouts {
        if pm.is_empty() {
            println!("  rs = {{}}  (no pool rewards — see diagnosis below)");
        } else {
            let mut entries: Vec<_> = pm.iter().collect();
            entries.sort_by_key(|(k, _)| k.clone());
            for (cred, amt) in &entries {
                println!("  {} : {}", abbrev(cred, 24), fmt_u64(amt.as_u64().unwrap_or(0)));
            }
        }
    }
    let ok6 = dump_total_distributed.map_or(true, |td| td == sigma_rs);
    println!("  Σrs = {}  {}", fmt_u64(sigma_rs), chk(ok6));
    if let Some(td) = dump_total_distributed { if !ok6 { println!("  dump totalDistributed: {}", fmt_u64(td)); } }
    println!();

    // Diagnosis when rs = {}
    if sigma_rs == 0 {
        println!("  Diagnosis (rs = {{}}):");
        let reward_accounts = n.get("rewardAccounts").and_then(|v| v.as_object());
        if let Some(go_v) = go {
            let pool_params = go_v.get("poolParameters")
                .or_else(|| go_v.get("poolParams"))
                .and_then(|v| v.as_object());
            if let Some(pp_map) = pool_params {
                for (pool_id, params) in pp_map {
                    let ra_cred = params.pointer("/rewardAccount/credential/keyHash")
                        .and_then(|v| v.as_str());
                    let margin = params.get("margin").and_then(|v| v.as_f64());
                    let pledge = params.get("pledge").and_then(|v| v.as_u64());
                    if let Some(cred) = ra_cred {
                        let ra_key = format!("keyHash-{}", &cred[..cred.len().min(56)]);
                        let registered = reward_accounts.map_or(false, |m| m.contains_key(&ra_key));
                        println!("  pool {}  ra={}  registered={}  margin={:?}  pledge={:?}",
                            abbrev(pool_id, 16), abbrev(&ra_key, 28), registered, margin, pledge);
                    }
                }
            }
        }
        if reward_accounts.is_none() {
            println!("  (rewardAccounts absent from dump — all pool reward accounts likely unregistered)");
        }
        println!();
    }

    // ── Step 6: Δr₂ ──
    println!("Step 6: Δr₂ = R - Σrs  (unspent pool rewards → reserves)");
    let delta_r2_computed = reward_pot.saturating_sub(sigma_rs);
    let ok7 = dump_delta_r2.map_or(true, |d| d == delta_r2_computed);
    println!("  = {} - {} = {}  {}",
        fmt_u64(reward_pot), fmt_u64(sigma_rs), fmt_u64(delta_r2_computed), chk(ok7));
    if let Some(d) = dump_delta_r2 { if !ok7 { println!("  dump: {}", fmt_u64(d)); } }
    let delta_r2 = dump_delta_r2.unwrap_or(delta_r2_computed);
    println!();

    // ── Carry-forward ──
    println!("=== Carry-forward: Epoch {} → {} ===\n", epoch_num, epoch_num + 1);

    // Reserves
    let reserves_expected = (reserves_n as i128 - delta_r1 as i128 + delta_r2 as i128) as u64;
    let res_ok = reserves_expected == reserves_n1;
    println!("reserves_{e1} = reserves_{e0} - Δr₁ + Δr₂",
        e0 = epoch_num, e1 = epoch_num + 1);
    println!("  = {} - {} + {}", fmt_u64(reserves_n), fmt_u64(delta_r1), fmt_u64(delta_r2));
    println!("  = {}  {}",
        fmt_u64(reserves_expected), chk_with_actual(res_ok, reserves_n1));
    println!();

    // Treasury — forward-compute unregRU' from N+1 registeredStakeAddresses
    let registered_n1: std::collections::HashSet<String> = n1
        .get("registeredStakeAddresses")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(|s| normalize_cred(s)).collect())
        .unwrap_or_default();
    let have_registered = n1.get("registeredStakeAddresses").is_some();
    let unreg_ru: u64 = dump_reward_payouts
        .map(|m| m.iter()
            .filter(|(cred, _)| !registered_n1.contains(&normalize_cred(cred)))
            .filter_map(|(_, v)| v.as_u64())
            .sum())
        .unwrap_or(0);
    let treasury_expected = treasury_n + delta_t1 + unreg_ru;
    let treas_ok = treasury_expected == treasury_n1;
    println!("treasury_{e1} = treasury_{e0} + Δt₁ + unregRU'",
        e0 = epoch_num, e1 = epoch_num + 1);
    println!("  unregRU' = Σ rs[cred] for cred ∉ registeredStakeAddresses_{e1} = {}", fmt_u64(unreg_ru), e1 = epoch_num + 1);
    if have_registered {
        let unregistered: Vec<_> = dump_reward_payouts
            .map(|m| m.iter()
                .filter(|(cred, _)| !registered_n1.contains(&normalize_cred(cred)))
                .map(|(cred, v)| (cred.clone(), v.as_u64().unwrap_or(0)))
                .collect())
            .unwrap_or_default();
        for (cred, amt) in &unregistered {
            println!("    {} : {}", abbrev(cred, 24), fmt_u64(*amt));
        }
    } else if dump_reward_payouts.is_some() {
        println!("  (registeredStakeAddresses not in N+1 dump — cannot identify unregistered credentials)");
    }
    println!("  = {} + {} + {} = {}  {}",
        fmt_u64(treasury_n), fmt_u64(delta_t1), fmt_u64(unreg_ru),
        fmt_u64(treasury_expected), chk_with_actual(treas_ok, treasury_n1));
    println!();

    // rupdNext == rupdApplied
    let applied = &n1["rupdApplied"];
    if !applied.is_null() {
        let keys = ["deltaR1", "deltaR2", "deltaT1", "rPot", "rewardPot", "totalDistributed"];
        let mismatches: Vec<_> = keys.iter().filter_map(|k| {
            let hn = rupd.get(k).and_then(|v| v.as_u64());
            let an = applied.get(k).and_then(|v| v.as_u64());
            if hn != an { Some((*k, hn, an)) } else { None }
        }).collect();
        println!("rupdNext == rupdApplied:  {}", chk(mismatches.is_empty()));
        for (k, hn, an) in &mismatches {
            println!("  {}: rupdNext={:?}  rupdApplied={:?}", k, hn, an);
        }
    } else {
        println!("rupdApplied: not present in epoch N+1 dump");
    }
    println!();

    if res_ok && treas_ok {
        println!("=== All carry-forward checks PASSED ✓ ===");
    } else {
        println!("=== DIVERGENCE DETECTED ✗ ===");
        std::process::exit(1);
    }

    Ok(())
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// floor(an/ad · bn/bd · val)
fn floor_mul3(an: u64, ad: u64, bn: u64, bd: u64, val: u128) -> u64 {
    ((an as u128 * bn as u128 * val) / (ad as u128 * bd as u128)) as u64
}

fn get_rational_param(pp: &Value, key: &str) -> Option<(u64, u64)> {
    let v = pp.get(key)?;
    if let (Some(n), Some(d)) = (
        v.get("numerator").and_then(|x| x.as_u64()),
        v.get("denominator").and_then(|x| x.as_u64()),
    ) {
        Some((n, d))
    } else if let Some(f) = v.as_f64() {
        float_to_rational(f)
    } else {
        None
    }
}

fn float_to_rational(f: f64) -> Option<(u64, u64)> {
    for den in &[1000u64, 500, 200, 100, 50, 20, 10, 5, 4, 3, 2, 1] {
        let num = (f * *den as f64).round() as u64;
        if (num as f64 / *den as f64 - f).abs() < 1e-9 {
            let g = gcd(num, *den);
            return Some((num / g, den / g));
        }
    }
    None
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn fmt_u64(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    s.chars().enumerate().fold(String::new(), |mut acc, (i, c)| {
        if i > 0 && (len - i) % 3 == 0 { acc.push(','); }
        acc.push(c);
        acc
    })
}

fn fmt_i128(n: i128) -> String {
    if n < 0 { format!("-{}", fmt_u64((-n) as u64)) } else { fmt_u64(n as u64) }
}

fn chk(ok: bool) -> &'static str {
    if ok { "✓" } else { "✗ MISMATCH" }
}

fn chk_with_actual(ok: bool, actual: u64) -> String {
    if ok { "✓".to_string() } else { format!("✗  actual: {}", fmt_u64(actual)) }
}

fn abbrev(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max]) }
}

// ─── compare ─────────────────────────────────────────────────────────────────

fn run_compare(
    haskell_dir: &PathBuf,
    hayate_dir: &PathBuf,
    from_epoch: u64,
    to_epoch: Option<u64>,
    keep_going: bool,
) -> Result<()> {
    let haskell_dumps = load_dir(haskell_dir, false)?;
    let hayate_dumps  = load_dir(hayate_dir, true)?;

    println!("Loaded {} Haskell dumps, {} Hayate dumps", haskell_dumps.len(), hayate_dumps.len());

    let max_epoch = to_epoch.unwrap_or_else(|| *haskell_dumps.keys().max().unwrap_or(&0));

    let mut all_ok = true;

    for epoch in from_epoch..=max_epoch {
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
        let hayate_next = hayate_dumps.get(&(epoch + 1));

        let mut critical: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        compare_u64(haskell, hayate, "treasury",    &mut critical);
        compare_u64(haskell, hayate, "reserves",    &mut critical);
        compare_u64(haskell, hayate, "epochFees",   &mut warnings);
        compare_u64(haskell, hayate, "activeStake", &mut critical);
        compare_deposits(haskell, hayate, &mut critical);
        compare_protocol_params(haskell, hayate, &mut critical);

        if let Some(hn) = hayate_next {
            compare_rupd(haskell, hn, epoch, &mut critical);
        }

        compare_era_name(haskell, hayate, &mut warnings);
        compare_conway_gov(haskell, hayate, &mut critical, &mut warnings);
        compare_snapshots(haskell, hayate, &mut critical, &mut warnings);

        if critical.is_empty() && warnings.is_empty() {
            println!("epoch {epoch}: OK");
        } else if critical.is_empty() {
            println!("epoch {epoch}: OK (warnings)");
            for w in &warnings { println!("  WARN: {w}"); }
        } else {
            println!("epoch {epoch}: DIVERGED");
            for m in &critical { println!("  CRITICAL: {m}"); }
            for w in &warnings { println!("  WARN: {w}"); }
            all_ok = false;
            if !keep_going {
                println!("\nStopped at first critical divergence (use --keep-going to continue).");
                break;
            }
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
        if !name.ends_with(".json") { continue; }
        let epoch = if is_hayate {
            name.strip_suffix("-hayate.json").and_then(|s| s.parse::<u64>().ok())
        } else {
            name.strip_suffix(".json")
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<u64>().ok())
        };
        if let Some(epoch) = epoch {
            let content = std::fs::read_to_string(entry.path())?;
            if content.trim().is_empty() { continue; }
            match serde_json::from_str(&content) {
                Ok(v) => { map.insert(epoch, v); }
                Err(e) => eprintln!("Warning: skipping {} ({})", entry.path().display(), e),
            }
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
                "deposits.{key}: haskell={h} hayate={r} diff={}", r as i128 - h as i128
            )),
            (None, Some(_)) => out.push(format!("deposits.{key}: missing in haskell")),
            (Some(_), None) => out.push(format!("deposits.{key}: missing in hayate")),
            (None, None) => {}
        }
    }
}

fn compare_protocol_params(haskell: &Value, hayate: &Value, out: &mut Vec<String>) {
    let hp = match haskell.get("protocolParams") {
        Some(v) if !v.is_null() => v,
        _ => return,
    };
    let rp = match hayate.get("protocolParams") {
        Some(v) if !v.is_null() => v,
        _ => return,
    };

    let h_nopt = hp.get("nOpt").and_then(|v| v.as_u64());
    let r_nopt = rp.get("nOpt").and_then(|v| v.as_u64());
    if h_nopt != r_nopt { out.push(format!("protocolParams.nOpt: haskell={h_nopt:?} hayate={r_nopt:?}")); }

    for key in &["rho", "tau", "a0", "d"] {
        let hv = hp.get(key).and_then(|v| v.as_f64());
        let rv = rp.get(key).and_then(|v| v.as_f64());
        match (hv, rv) {
            (Some(h), Some(r)) if (h - r).abs() > 1e-9 =>
                out.push(format!("protocolParams.{key}: haskell={h} hayate={r}")),
            _ => {}
        }
    }
}

fn compare_rupd(haskell: &Value, hayate_next: &Value, epoch: u64, out: &mut Vec<String>) {
    let hr = haskell.get("rupdNext")
        .filter(|v| !v.is_null())
        .unwrap_or(&haskell["rupd"]);
    let rr = &hayate_next["rupd"];

    if hr.is_null() { out.push(format!("rupdNext: missing in haskell[{epoch}]")); return; }
    if rr.is_null() { out.push(format!("rupd: missing in hayate[{}]", epoch + 1)); return; }

    let prefix = format!("rupdNext (haskell[{epoch}] vs hayate[{}])", epoch + 1);
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

fn normalize_cred(s: &str) -> String {
    let hex_part = s.strip_prefix("keyHash-").unwrap_or(s);
    format!("keyHash-{}", &hex_part[..56.min(hex_part.len())])
}

fn compare_era_name(haskell: &Value, hayate: &Value, out: &mut Vec<String>) {
    let h = haskell.get("snapshotEraName").and_then(|v| v.as_str());
    let r = hayate.get("snapshotEraName").and_then(|v| v.as_str());
    if let (Some(h), Some(r)) = (h, r) {
        if h != r { out.push(format!("snapshotEraName: haskell={h} hayate={r}")); }
    }
}

fn compare_conway_gov(haskell: &Value, hayate: &Value, critical: &mut Vec<String>, warnings: &mut Vec<String>) {
    let hg = haskell.get("conwayGov");
    let rg = hayate.get("conwayGov");

    match (hg, rg) {
        (None | Some(Value::Null), None | Some(Value::Null)) => return,
        (Some(hv), None | Some(Value::Null)) if !hv.is_null() => {
            critical.push("conwayGov: present in haskell but null in hayate".to_string());
            return;
        }
        (None | Some(Value::Null), Some(rv)) if !rv.is_null() => {
            warnings.push("conwayGov: present in hayate but null in haskell".to_string());
            return;
        }
        _ => {}
    }

    let hg = match hg.and_then(|v| if v.is_null() { None } else { Some(v) }) { Some(v) => v, None => return };
    let rg = match rg.and_then(|v| if v.is_null() { None } else { Some(v) }) { Some(v) => v, None => return };

    let h_members = hg.pointer("/committee/members").and_then(|v| v.as_object()).map(|m| m.len());
    let r_members = rg.pointer("/committee/members").and_then(|v| v.as_object()).map(|m| m.len());
    if h_members != r_members {
        warnings.push(format!("conwayGov.committee.members count: haskell={h_members:?} hayate={r_members:?}"));
    }

    let h_thresh_n = hg.pointer("/committee/threshold/numerator").and_then(|v| v.as_u64());
    let r_thresh_n = rg.pointer("/committee/threshold/numerator").and_then(|v| v.as_u64());
    let h_thresh_d = hg.pointer("/committee/threshold/denominator").and_then(|v| v.as_u64());
    let r_thresh_d = rg.pointer("/committee/threshold/denominator").and_then(|v| v.as_u64());
    if h_thresh_n != r_thresh_n || h_thresh_d != r_thresh_d {
        warnings.push(format!(
            "conwayGov.committee.threshold: haskell={h_thresh_n:?}/{h_thresh_d:?} hayate={r_thresh_n:?}/{r_thresh_d:?}"
        ));
    }

    let h_url = hg.pointer("/constitution/anchor/url").and_then(|v| v.as_str());
    let r_url = rg.pointer("/constitution/anchor/url").and_then(|v| v.as_str());
    match (h_url, r_url) {
        (Some(h), Some(r)) if h != r => critical.push(format!("conwayGov.constitution.url: haskell={h} hayate={r}")),
        (Some(_), None) => warnings.push("conwayGov.constitution.url: missing in hayate".to_string()),
        (None, Some(_)) => warnings.push("conwayGov.constitution.url: missing in haskell".to_string()),
        _ => {}
    }

    let drep_total = |gov: &Value| -> u64 {
        gov.get("drepDistr").and_then(|v| v.as_object())
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum()).unwrap_or(0)
    };
    let h_drep_count = hg.get("drepDistr").and_then(|v| v.as_object()).map(|m| m.len());
    let r_drep_count = rg.get("drepDistr").and_then(|v| v.as_object()).map(|m| m.len());
    if h_drep_count != r_drep_count {
        critical.push(format!("conwayGov.drepDistr entry count: haskell={h_drep_count:?} hayate={r_drep_count:?}"));
    }
    let h_drep_total = drep_total(hg);
    let r_drep_total = drep_total(rg);
    if h_drep_total != r_drep_total {
        critical.push(format!(
            "conwayGov.drepDistr total stake: haskell={h_drep_total} hayate={r_drep_total} diff={}",
            r_drep_total as i128 - h_drep_total as i128
        ));
    }

    let nes  = hg.get("nextEnactState");
    let rnes = rg.get("nextEnactState");
    if let (Some(hnes), Some(rnes)) = (nes, rnes) {
        for action_type in &["Committee", "Constitution", "HardFork", "PParamUpdate"] {
            let h_id = hnes.pointer(&format!("/prevGovActionIds/{action_type}/txId")).and_then(|v| v.as_str());
            let r_id = rnes.pointer(&format!("/prevGovActionIds/{action_type}/txId")).and_then(|v| v.as_str());
            match (h_id, r_id) {
                (Some(h), Some(r)) if h != r => warnings.push(format!(
                    "conwayGov.nextEnactState.prevGovActionIds.{action_type}: haskell={h} hayate={r}"
                )),
                (Some(_), None) => warnings.push(format!(
                    "conwayGov.nextEnactState.prevGovActionIds.{action_type}: present in haskell, null in hayate"
                )),
                _ => {}
            }
        }
    }
}

fn snapshot_stake_total(snap: &Value) -> u64 {
    snap.get("stake").and_then(|s| s.as_object())
        .map(|m| m.values().filter_map(|v| v.as_u64()).sum()).unwrap_or(0)
}

fn compare_snapshots(haskell: &Value, hayate: &Value, critical: &mut Vec<String>, warnings: &mut Vec<String>) {
    for snap_name in &["mark", "set", "go"] {
        let hs = haskell.get("snapshots").and_then(|s| s.get(snap_name));
        let rs = hayate.get("snapshots").and_then(|s| s.get(snap_name));
        match (hs, rs) {
            (None, None) => {}
            (None, Some(_)) => warnings.push(format!("snapshots.{snap_name}: missing in haskell")),
            (Some(_), None) => critical.push(format!("snapshots.{snap_name}: missing in hayate")),
            (Some(h), Some(r)) => {
                if h.is_null() && r.is_null() { continue; }

                let ht = snapshot_stake_total(h);
                let rt = snapshot_stake_total(r);
                if ht != rt {
                    critical.push(format!(
                        "snapshots.{snap_name}.totalStake: haskell={ht} hayate={rt} diff={}",
                        rt as i128 - ht as i128
                    ));
                }

                let hstake: HashMap<String, u64> = h.get("stake").and_then(|s| s.as_object())
                    .map(|m| m.iter().map(|(k, v)| (normalize_cred(k), v.as_u64().unwrap_or(0))).collect())
                    .unwrap_or_default();
                let rstake: HashMap<String, u64> = r.get("stake").and_then(|s| s.as_object())
                    .map(|m| m.iter().map(|(k, v)| (normalize_cred(k), v.as_u64().unwrap_or(0))).collect())
                    .unwrap_or_default();
                for (cred, hv) in &hstake {
                    let rv = rstake.get(cred).copied().unwrap_or(0);
                    if *hv != rv { critical.push(format!("snapshots.{snap_name}.stake[{cred}]: haskell={hv} hayate={rv}")); }
                }
                for (cred, rv) in &rstake {
                    if !hstake.contains_key(cred) {
                        critical.push(format!("snapshots.{snap_name}.stake[{cred}]: missing in haskell, hayate={rv}"));
                    }
                }

                let hparams = h.get("poolParams").and_then(|v| v.as_object());
                let rparams = r.get("poolParams").and_then(|v| v.as_object());
                if let (Some(hp), Some(rp)) = (hparams, rparams) {
                    for (pool_id, hpool) in hp {
                        let pool_key = &pool_id[..56.min(pool_id.len())];
                        if let Some(rpool) = rp.get(pool_key).or_else(|| rp.get(pool_id.as_str())) {
                            for field in &["pledge", "cost"] {
                                let hv = hpool.get(field).and_then(|v| v.as_u64());
                                let rv = rpool.get(field).and_then(|v| v.as_u64());
                                if hv != rv { critical.push(format!("snapshots.{snap_name}.poolParams[{pool_key}].{field}: haskell={hv:?} hayate={rv:?}")); }
                            }
                            let hm = hpool.get("margin").and_then(|v| v.as_f64());
                            let rm = rpool.get("margin").and_then(|v| v.as_f64());
                            if let (Some(h), Some(r)) = (hm, rm) {
                                if (h - r).abs() > 1e-9 {
                                    critical.push(format!("snapshots.{snap_name}.poolParams[{pool_key}].margin: haskell={h} hayate={r}"));
                                }
                            }
                        } else {
                            critical.push(format!("snapshots.{snap_name}.poolParams[{pool_key}]: missing in hayate"));
                        }
                    }
                    for (pool_id, _) in rp {
                        let pool_key = &pool_id[..56.min(pool_id.len())];
                        if !hp.contains_key(pool_id.as_str()) && !hp.contains_key(pool_key) {
                            critical.push(format!("snapshots.{snap_name}.poolParams[{pool_key}]: extra pool in hayate"));
                        }
                    }
                }
            }
        }
    }
}
