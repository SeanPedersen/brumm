//! Command-line arguments (clap). Every flag also works as an environment variable where that
//! helps in cron jobs and containers (`BRUMM_PROFILE`, `BRUMM_DATA_DIR`, …).

use std::{path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{paths::DEFAULT_PROFILE, text::parse_duration};

#[derive(Debug, Parser)]
#[command(
    name = "brumm-cli",
    version,
    about = "Headless brumm for software, bots and agents",
    long_about = "Headless brumm for software, bots and agents.\n\n\
        Commands go to a background daemon (started on demand, exits when idle) unless\n\
        --no-daemon runs them in this process. Rooms are named by contact name, group name,\n\
        `notes`, `me` (the first contact, e.g. a bot's owner) or room id (d:… / g:…)."
)]
pub struct Cli {
    /// Identity to use; each profile is its own directory and brumm user.
    #[arg(long, short = 'p', global = true, env = "BRUMM_PROFILE", default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Data root holding `profiles/` (default: the desktop app's).
    #[arg(long, global = true, env = "BRUMM_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Daemon socket (default: per data root, in $XDG_RUNTIME_DIR/brumm).
    #[arg(long, global = true, env = "BRUMM_SOCKET")]
    pub socket: Option<PathBuf>,
    /// Machine-readable output: one JSON value per line.
    #[arg(long, global = true)]
    pub json: bool,
    /// Run in this process instead of the daemon (one-shot: drains and exits).
    #[arg(long, global = true, env = "BRUMM_NO_DAEMON")]
    pub no_daemon: bool,
    /// Nostr relays this profile receives offline messages on (default: public relays).
    #[arg(long = "relay", global = true, env = "BRUMM_RELAYS", value_delimiter = ',')]
    pub relays: Vec<String>,
    /// How often undelivered messages are resent (tests).
    #[arg(long, global = true, hide = true, env = "BRUMM_RETRY_INTERVAL", value_parser = parse_duration)]
    pub retry_interval: Option<Duration>,
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create a new identity for the profile (prints its seed phrase once).
    Init { name: String },
    /// Restore an identity from its seed phrase (as a new device; read from stdin if not given).
    Restore {
        name: String,
        #[arg(long)]
        phrase: Option<String>,
    },
    /// Show the profile's identity.
    Show,
    /// Create a single-use invite link (and QR) and wait until it is used.
    Invite {
        #[arg(long)]
        no_qr: bool,
        /// Print the link and exit (the invite only works while this profile is online).
        #[arg(long)]
        no_wait: bool,
        #[arg(long, default_value = "10m", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Accept an invite link (the inviting app must be online).
    Accept {
        link: String,
        /// Lock the profile afterwards: no more invites or contacts (bots).
        #[arg(long)]
        lock: bool,
    },
    /// List contacts.
    Contacts,
    /// List rooms (Notes, contacts, groups).
    Rooms,
    /// List pending group invitations.
    Invitations,
    /// List profiles under the data root.
    Profiles,
    /// Send a message (text from the arguments or stdin, or a file).
    Send(SendArgs),
    /// Print messages of a room after a cursor (all from the first by default).
    Read {
        room: String,
        /// Cursor from an earlier read.
        #[arg(long, conflicts_with = "last")]
        since: Option<u64>,
        #[arg(long)]
        limit: Option<u32>,
        /// Only the last N messages.
        #[arg(long)]
        last: Option<u32>,
    },
    /// Print incoming messages as they arrive (all rooms if none are given).
    Listen(ListenArgs),
    /// Wait until a sent message reached a status.
    Wait {
        room: String,
        id: String,
        #[arg(long, value_enum, default_value_t = WaitStatus::Delivered)]
        status: WaitStatus,
        #[arg(long, default_value = "60s", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Groups (agent rooms).
    #[command(subcommand)]
    Group(GroupCmd),
    /// Show or change the profile's policy (auto-accept, lock mode).
    Policy(PolicyArgs),
    /// Run or control the daemon (`brumm-cli daemon` runs it in the foreground).
    Daemon(DaemonArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WaitStatus {
    None,
    Sent,
    Delivered,
}

impl WaitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
        }
    }
}

#[derive(Debug, Args)]
pub struct SendArgs {
    pub room: String,
    /// The text; `-` or nothing reads stdin.
    pub text: Vec<String>,
    #[arg(long, conflicts_with = "text")]
    pub file: Option<PathBuf>,
    /// Your own message id (base32, 16 bytes): a retry with it sends nothing twice.
    #[arg(long)]
    pub id: Option<String>,
    /// How far to wait before exiting.
    #[arg(long, value_enum, default_value_t = WaitStatus::Sent)]
    pub wait: WaitStatus,
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub timeout: Duration,
    /// Text over the 16 KiB limit: send several messages (default: one file).
    #[arg(long)]
    pub split: bool,
}

#[derive(Debug, Args)]
pub struct ListenArgs {
    pub rooms: Vec<String>,
    /// Start after this cursor (one room only) instead of now.
    #[arg(long)]
    pub since: Option<u64>,
    /// Exit after this many messages.
    #[arg(long)]
    pub count: Option<usize>,
    /// Exit after this long.
    #[arg(long, value_parser = parse_duration)]
    pub timeout: Option<Duration>,
    /// Also print our own messages (sent from this or another device).
    #[arg(long)]
    pub outgoing: bool,
    /// Also print status changes, invitations and room changes.
    #[arg(long)]
    pub all_events: bool,
}

#[derive(Debug, Subcommand)]
pub enum GroupCmd {
    /// Create a group with you as owner and invite contacts.
    Create {
        name: String,
        members: Vec<String>,
        /// Only owner and admins may post.
        #[arg(long)]
        announcement: bool,
    },
    Invite { room: String, users: Vec<String> },
    /// Accept a group invitation (by group name or room id).
    Accept { room: String },
    Decline { room: String },
    Leave { room: String },
}

#[derive(Debug, Args)]
pub struct PolicyArgs {
    /// Refuse new invites and contacts; group invitations only from the allowlist.
    #[arg(long, conflicts_with = "unlock")]
    pub lock: bool,
    #[arg(long)]
    pub unlock: bool,
    /// Add a user (contact name or user key) whose group invitations are accepted.
    #[arg(long)]
    pub allow: Vec<String>,
    /// Empty the allowlist (then the first contact is allowed).
    #[arg(long)]
    pub clear_allow: bool,
    /// Accept invitations from allowed users automatically.
    #[arg(long)]
    pub auto_accept: Option<bool>,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: Option<DaemonCmd>,
    /// Exit after no client was connected for this long; idle profiles close too.
    #[arg(long, global = true, value_parser = parse_duration)]
    pub idle_exit: Option<Duration>,
    #[arg(long, global = true, default_value_t = crate::host::DEFAULT_MAX_PROFILES)]
    pub max_profiles: usize,
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum DaemonCmd {
    /// Run in the foreground (systemd).
    Run,
    /// Start in the background unless one runs; prints its status.
    Start,
    Stop,
    Status,
}
