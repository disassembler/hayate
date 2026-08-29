// CLI argument parsing for Hayate

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "hayate")]
#[command(about = "疾風 Hayate - Swift Cardano indexer with UTxORPC", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Configuration file
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    /// Database directory (overrides config)
    #[arg(short, long, global = true)]
    pub db_path: Option<String>,

    /// Network to use (mainnet, preprod, preview, sanchonet)
    #[arg(short, long, global = true)]
    pub network: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the indexer and sync from the network
    Sync {
        /// UTxORPC API bind address (overrides config)
        #[arg(long)]
        api_bind: Option<String>,

        /// Gap limit for address discovery (overrides config)
        #[arg(long)]
        gap_limit: Option<u32>,

        /// Start from genesis
        #[arg(long)]
        from_genesis: bool,

        /// Node socket path (for direct node connection)
        #[arg(short, long)]
        socket: Option<String>,
    },

    /// Wallet query commands
    Wallet {
        #[command(subcommand)]
        wallet_cmd: WalletCommand,
    },

    /// Query blockchain data
    Query {
        #[command(subcommand)]
        query_cmd: QueryCommand,
    },

    /// Configuration commands
    Config {
        #[command(subcommand)]
        config_cmd: ConfigCommand,
    },

    /// Rollback to a specific epoch
    Rollback {
        /// Target epoch to rollback to
        #[arg(short, long)]
        epoch: u64,

        /// Network to rollback (preview, preprod, mainnet, sanchonet)
        #[arg(short, long)]
        network: Option<String>,

        /// Database path
        #[arg(short = 'd', long)]
        db_path: Option<String>,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(ValueEnum, Clone, Debug)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

#[derive(Subcommand, Debug)]
pub enum WalletCommand {
    /// Initialize a new wallet with mnemonic
    Init {
        /// Wallet name
        name: String,

        /// GPG recipient for encryption (email or key ID)
        #[arg(long)]
        gpg_recipient: Option<String>,

        /// Number of mnemonic words (12, 15, 18, 21, or 24)
        #[arg(long, default_value = "24")]
        words: usize,

        /// Network (mainnet or testnet)
        #[arg(long, default_value = "testnet")]
        network: String,
    },

    /// Add existing wallet from mnemonic
    Add {
        /// Wallet name
        name: String,

        /// Mnemonic phrase (will prompt if not provided)
        #[arg(long)]
        mnemonic: Option<String>,

        /// Mnemonic file path (supports GPG encryption)
        #[arg(long)]
        mnemonic_file: Option<std::path::PathBuf>,

        /// GPG recipient for encryption (email or key ID)
        #[arg(long)]
        gpg_recipient: Option<String>,

        /// Network (mainnet or testnet)
        #[arg(long, default_value = "testnet")]
        network: String,
    },

    /// List all wallets
    List,

    /// Show wallet details and addresses
    Show {
        /// Wallet name
        name: String,

        /// Number of addresses to show
        #[arg(long, default_value = "5")]
        count: u32,
    },

    /// Export wallet mnemonic (WARNING: sensitive operation!)
    Export {
        /// Wallet name
        name: String,
    },

    /// Delete a wallet
    Delete {
        /// Wallet name
        name: String,

        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },

    /// Show wallet statistics (UTxOs, balance, transactions)
    Stats {
        /// Wallet xpub or identifier (if not specified, shows all wallets)
        wallet: Option<String>,
    },

    /// List wallet UTxOs
    Utxos {
        /// Wallet xpub or identifier
        wallet: String,
    },

    /// List wallet transaction history
    Txs {
        /// Wallet xpub or identifier
        wallet: String,
    },

    // Transaction commands
    /// Send ADA to an address
    SendTx {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Recipient address
        #[arg(long)]
        address: String,

        /// Amount in lovelace
        #[arg(long)]
        amount: u64,

        /// Transaction fee in lovelace (optional - will be calculated automatically if not provided)
        #[arg(long)]
        fee: Option<u64>,

        /// Node socket path (required for automatic fee calculation)
        #[arg(long)]
        socket: Option<String>,

        /// Network magic number (required for automatic fee calculation)
        #[arg(long)]
        magic: Option<u64>,

        /// Output file for transaction
        #[arg(long)]
        out_file: String,

        /// Include native assets
        #[arg(long)]
        multiasset: bool,

        /// TTL (time to live) slot
        #[arg(long)]
        ttl: Option<u64>,

        /// Sign the transaction
        #[arg(long)]
        sign: bool,
    },

    /// Drain all funds from an account
    DrainTx {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Destination address
        #[arg(long)]
        address: String,

        /// Transaction fee in lovelace (optional - will be calculated automatically if not provided)
        #[arg(long)]
        fee: Option<u64>,

        /// Node socket path (required for automatic fee calculation)
        #[arg(long)]
        socket: Option<String>,

        /// Network magic number (required for automatic fee calculation)
        #[arg(long)]
        magic: Option<u64>,

        /// Output file for transaction
        #[arg(long)]
        out_file: String,

        /// Include native assets
        #[arg(long)]
        multiasset: bool,

        /// Include staking rewards
        #[arg(long)]
        rewards: bool,

        /// TTL (time to live) slot
        #[arg(long)]
        ttl: Option<u64>,

        /// Sign the transaction
        #[arg(long)]
        sign: bool,
    },

    /// Create stake key registration transaction
    StakeRegistrationTx {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Transaction fee in lovelace
        #[arg(long)]
        fee: u64,

        /// Output file for transaction
        #[arg(long)]
        out_file: String,

        /// Registration deposit (default: 2000000 lovelace)
        #[arg(long, default_value = "2000000")]
        deposit: u64,

        /// TTL (time to live) slot
        #[arg(long)]
        ttl: Option<u64>,

        /// Sign the transaction
        #[arg(long)]
        sign: bool,
    },

    /// Create stake pool delegation transaction
    DelegatePoolTx {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Pool ID (bech32)
        #[arg(long)]
        pool_id: String,

        /// Transaction fee in lovelace
        #[arg(long)]
        fee: u64,

        /// Output file for transaction
        #[arg(long)]
        out_file: String,

        /// TTL (time to live) slot
        #[arg(long)]
        ttl: Option<u64>,

        /// Sign the transaction
        #[arg(long)]
        sign: bool,
    },

    /// Sign a transaction body
    SignTx {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Transaction body file
        #[arg(long)]
        tx_body_file: String,

        /// Output file for signed transaction
        #[arg(long)]
        out_file: String,

        /// Sign with stake key as well
        #[arg(long)]
        stake: bool,
    },

    /// Create a transaction witness
    WitnessTx {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Transaction body file
        #[arg(long)]
        tx_body_file: String,

        /// Output file for witness
        #[arg(long)]
        out_file: String,

        /// Witness type (payment or stake)
        #[arg(long, default_value = "payment")]
        role: String,
    },

    /// Sign a message (CIP-8)
    SignMsg {
        /// Wallet name
        #[arg(long)]
        wallet: String,

        /// Account index
        #[arg(long, default_value = "0")]
        account: u32,

        /// Message file to sign
        #[arg(long)]
        msg_file: String,

        /// Output file for JSON signature
        #[arg(long)]
        out_file: String,

        /// Use stake key instead of payment key
        #[arg(long)]
        stake: bool,

        /// Hash the message before signing
        #[arg(long)]
        hashed: bool,
    },

    /// CIP-1854 multi-signature wallet operations
    Multisig {
        #[command(subcommand)]
        multisig_cmd: MultisigCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum MultisigCommand {
    /// Print the payment address for a CIP-1854 key in a local wallet
    PaymentAddress {
        /// Local wallet name
        #[arg(long)]
        wallet: String,

        /// Account index for CIP-1854 key derivation
        #[arg(long, default_value = "0")]
        wallet_account: u32,

        /// Key index within role 0
        #[arg(long, default_value = "0")]
        wallet_key_index: u32,

        /// Network (mainnet, preprod, preview, testnet)
        #[arg(long, default_value = "testnet")]
        network: String,
    },

    /// Derive a CIP-1854 M-of-N multisig enterprise address
    CreateAddress {
        /// Local wallet name contributing a CIP-1854 payment key; may be specified multiple times
        #[arg(long = "wallet", action = clap::ArgAction::Append)]
        wallets: Vec<String>,

        /// Account index for each local wallet (positional, defaults to 0)
        #[arg(long = "wallet-account", action = clap::ArgAction::Append)]
        wallet_accounts: Vec<u32>,

        /// Key index within role 0 for each local wallet (positional, defaults to 0)
        #[arg(long = "wallet-key-index", action = clap::ArgAction::Append)]
        wallet_key_indices: Vec<u32>,

        /// External cosigner payment address (addr1.../addr_test1...); may be specified multiple times
        #[arg(long = "address", action = clap::ArgAction::Append)]
        addresses: Vec<String>,

        /// Required number of signatures (M in M-of-N)
        #[arg(long)]
        threshold: u32,

        /// Network (mainnet, testnet, preprod, preview, sanchonet)
        #[arg(long, default_value = "testnet")]
        network: String,

        /// Output file for native script policy (cardano-cli JSON format)
        #[arg(long)]
        policy_file: String,

        /// Sort key hashes lexicographically before building the script (matches MeshJS portal ordering)
        #[arg(long)]
        sort_keys: bool,
    },

    /// Sign a Conway era transaction with a CIP-1854 payment key, producing a VKey witness
    Sign {
        /// Local wallet name
        #[arg(long)]
        wallet: String,

        /// Account index for CIP-1854 key derivation
        #[arg(long, default_value = "0")]
        wallet_account: u32,

        /// Key index within role 0 for CIP-1854 key
        #[arg(long, default_value = "0")]
        wallet_key_index: u32,

        /// Transaction file in cardano-cli JSON format (Tx ConwayEra or TxBody ConwayEra)
        #[arg(long, conflicts_with = "tx_cbor")]
        tx: Option<String>,

        /// Raw transaction CBOR as hex (e.g. from MeshJS); alternative to --tx
        #[arg(long, conflicts_with = "tx")]
        tx_cbor: Option<String>,

        /// Output file for witness (cardano-cli TxWitness ConwayEra JSON format)
        #[arg(long)]
        out_file: String,
    },

    /// MeshJS multisig portal integration (fetch pending txs, submit witnesses)
    Portal {
        #[command(subcommand)]
        portal_cmd: PortalCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum PortalCommand {
    /// Step 1 (live machine, needs network): register bot and fetch nonce.
    /// Saves an init file to transfer via USB to the air-gapped signing machine.
    Init {
        /// Display name shown in the portal for this signer slot
        #[arg(long)]
        name: String,

        /// Your CIP-1854 signer address (from 'wallet multisig create-address' individual signer output)
        #[arg(long)]
        address: String,

        /// MeshJS portal base URL
        #[arg(long, default_value = "https://multisig.meshjs.dev")]
        portal_url: String,

        /// Output file for the init data (copy to air-gapped machine)
        #[arg(long, default_value = "portal-init.json")]
        out: String,
    },

    /// Step 2 (air-gapped machine, no network): sign the nonce with your CIP-1854 key.
    /// Reads the init file from Step 1, produces a signed file to transfer back.
    SignSetup {
        /// Local wallet name
        #[arg(long)]
        wallet: String,

        /// Account index used for the CIP-1854 key
        #[arg(long, default_value = "0")]
        wallet_account: u32,

        /// Key index used for the CIP-1854 key
        #[arg(long, default_value = "0")]
        wallet_key_index: u32,

        /// Init file from Step 1
        #[arg(long, default_value = "portal-init.json")]
        in_file: String,

        /// Output file for the signed data (copy back to live machine)
        #[arg(long, default_value = "portal-signed.json")]
        out: String,
    },

    /// Step 3 (live machine, needs network): submit signature, claim bot, save credentials.
    /// Reads the signed file from Step 2. Credentials file is used for all future fetch/submit-witness.
    CompleteSetup {
        /// Signed file from Step 2
        #[arg(long, default_value = "portal-signed.json")]
        signed: String,

        /// Output file for bot credentials
        #[arg(long, default_value = "portal-creds.json")]
        creds: String,
    },

    /// Create a multisig wallet on the portal (run once, gives you a wallet ID)
    CreateWallet {
        /// Display name for the wallet on the portal
        #[arg(long)]
        name: String,

        /// Signer address (from 'wallet multisig create-address'); repeat for each signer
        #[arg(long = "signer", action = clap::ArgAction::Append, required = true)]
        signers: Vec<String>,

        /// Required number of signatures (M in M-of-N)
        #[arg(long)]
        threshold: u32,

        /// Network (mainnet or testnet)
        #[arg(long, default_value = "testnet")]
        network: String,

        /// Optional description shown on the portal
        #[arg(long)]
        description: Option<String>,

        /// Bot credentials file
        #[arg(long, default_value = "portal-creds.json")]
        creds: String,
    },

    /// List multisig wallets the bot belongs to (shows wallet IDs)
    Wallets {
        /// Bot credentials file
        #[arg(long, default_value = "portal-creds.json")]
        creds: String,
    },

    /// Step 1: build an unsigned transaction and save it locally (no network needed).
    /// Transfer the output file to the air-gapped machine and sign it, then use propose-tx.
    BuildTx {
        /// Input UTxO in the form txid#index:lovelace; repeat for multiple inputs
        #[arg(long = "utxo", action = clap::ArgAction::Append, required = true)]
        utxos: Vec<String>,

        /// Recipient address
        #[arg(long)]
        to: String,

        /// Amount to send in lovelace
        #[arg(long)]
        amount: u64,

        /// Change address (the multisig script address)
        #[arg(long)]
        change: String,

        /// Transaction fee in lovelace
        #[arg(long)]
        fee: u64,

        /// Native script policy file (output of 'wallet multisig create-address')
        #[arg(long)]
        policy_file: String,

        /// TTL in slots (optional)
        #[arg(long)]
        ttl: Option<u64>,

        /// Network (mainnet or testnet)
        #[arg(long, default_value = "testnet")]
        network: String,

        /// Output file for the unsigned transaction
        #[arg(long, default_value = "unsigned-tx.json")]
        out: String,
    },

    /// Step 2: embed your witness into the unsigned tx and propose it to the portal.
    /// Run after signing the output of build-tx on the air-gapped machine.
    ProposeTx {
        /// Unsigned tx file (output of build-tx)
        #[arg(long, default_value = "unsigned-tx.json")]
        tx_file: String,

        /// Witness file (output of 'wallet multisig sign' on the air-gapped machine)
        #[arg(long)]
        witness_file: String,

        /// Portal wallet ID (from 'portal wallets')
        #[arg(long)]
        wallet_id: String,

        /// Description shown in the portal UI
        #[arg(long)]
        description: Option<String>,

        /// Bot credentials file
        #[arg(long, default_value = "portal-creds.json")]
        creds: String,
    },

    /// Fetch pending transactions for a wallet from the portal
    Fetch {
        /// Wallet ID (from the portal)
        #[arg(long)]
        wallet_id: String,

        /// Bot credentials file
        #[arg(long, default_value = "portal-creds.json")]
        creds: String,

        /// Directory to save pending transactions as JSON files
        #[arg(long)]
        out_dir: Option<String>,
    },

    /// Submit a VKey witness to the portal
    SubmitWitness {
        /// Wallet ID (from the portal)
        #[arg(long)]
        wallet_id: String,

        /// Transaction ID to submit witness for
        #[arg(long)]
        transaction_id: String,

        /// Witness JSON file (output of 'wallet multisig sign')
        #[arg(long)]
        witness_file: String,

        /// Bot credentials file
        #[arg(long, default_value = "portal-creds.json")]
        creds: String,

        /// Do not broadcast even when threshold is reached
        #[arg(long)]
        no_broadcast: bool,
    },

}

#[derive(Subcommand, Debug)]
pub enum QueryCommand {
    /// Query current protocol parameters
    ProtocolParams {
        /// Node socket path (required for querying)
        #[arg(short, long)]
        socket: String,

        /// Network magic number (1=preprod, 2=preview, 4=sanchonet, 764824073=mainnet)
        #[arg(short, long)]
        magic: u64,

        /// Output file path (stdout if not specified)
        #[arg(short, long)]
        output: Option<String>,

        /// Format output as JSON
        #[arg(long, default_value = "true")]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Generate default configuration file
    Generate {
        /// Output path for config file
        #[arg(default_value = "hayate-config.toml")]
        output: String,
    },
}
