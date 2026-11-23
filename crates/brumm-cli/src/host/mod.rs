//! The profile registry shared by the daemon and one-shot runs: profiles open lazily on first
//! use, each with its `policy.toml` applied, and close again when idle (daemons with
//! `--idle-exit`). Concurrent first requests for one profile open it once: each name has its
//! own async slot, so a slow open (waiting out another process's drain) blocks only that name.
//!
//! Requests reach the profiles through [`methods::call`] with a [`session::Session`].

pub mod methods;
pub mod session;

use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use brumm_core::node::NodeConfig;
use brumm_host::{Client, Handle, enforcer, policy};
use tokio::task::JoinHandle;

use crate::{
    paths,
    protocol::{RpcError, code},
};

/// Open profiles per host; each runs an iroh endpoint and Nostr subscriptions. Measured
/// (release, no relay traffic): ~10 MB daemon, +23 MB for the first profile, +4 MB for each
/// further one, so 16 stay around 100 MB.
pub const DEFAULT_MAX_PROFILES: usize = 16;
/// Text of `ProfileLock`'s error when another process holds the profile.
const LOCKED_BY_OTHER: &str = "another brumm instance";

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub root: PathBuf,
    /// Nostr relays our devices receive on (the core's defaults if `None`).
    pub relays: Option<Vec<String>>,
    pub max_profiles: usize,
    /// How often the core resends undelivered envelopes (the core's default if `None`).
    pub retry_interval: Option<Duration>,
}

pub struct Host {
    config: HostConfig,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

type Slot = tokio::sync::Mutex<Option<Arc<Profile>>>;

/// One open profile.
pub struct Profile {
    pub name: String,
    pub handle: Handle,
    pub policy: enforcer::SharedPolicy,
    client: Mutex<Option<Client>>,
    enforcer: JoinHandle<()>,
    usage: Mutex<Usage>,
}

struct Usage {
    /// Requests in flight and live subscriptions.
    active: usize,
    last_used: Instant,
}

/// A profile in use; the profile counts as idle only once every guard is gone.
pub struct InUse(Arc<Profile>);

impl InUse {
    fn new(profile: Arc<Profile>) -> Self {
        profile.usage().active += 1;
        Self(profile)
    }
}

impl Clone for InUse {
    fn clone(&self) -> Self {
        Self::new(self.0.clone())
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut usage = self.0.usage();
        usage.active -= 1;
        usage.last_used = Instant::now();
    }
}

impl Deref for InUse {
    type Target = Profile;

    fn deref(&self) -> &Profile {
        &self.0
    }
}

impl Profile {
    fn usage(&self) -> std::sync::MutexGuard<'_, Usage> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn idle_for(&self) -> Option<Duration> {
        let usage = self.usage();
        (usage.active == 0).then(|| usage.last_used.elapsed())
    }

    pub fn policy(&self) -> policy::Policy {
        self.policy.borrow().clone()
    }

    async fn close(&self) {
        self.enforcer.abort();
        let client = self.client.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(client) = client
            && let Err(err) = client.shutdown().await
        {
            tracing::warn!(profile = self.name, "closing failed: {err:#}");
        }
    }
}

impl Host {
    pub fn new(config: HostConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            slots: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.config.root
    }

    fn slot(&self, name: &str) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    fn slots(&self) -> Vec<(String, Arc<Slot>)> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, slot)| (name.clone(), slot.clone()))
            .collect()
    }

    /// The profile, opened on first use.
    pub async fn profile(&self, name: &str) -> Result<InUse, RpcError> {
        let data_dir = paths::profile_dir(&self.config.root, name).map_err(RpcError::invalid_params)?;
        let slot = self.slot(name);
        let mut open = slot.lock().await;
        if let Some(profile) = open.as_ref() {
            return Ok(InUse::new(profile.clone()));
        }
        if self.open_profiles().len() >= self.config.max_profiles {
            return Err(RpcError::new(
                code::BUSY,
                format!("{} profiles are open already (--max-profiles)", self.config.max_profiles),
            ));
        }
        let policy = policy::load(&data_dir).map_err(RpcError::from)?;
        let mut config = NodeConfig::new(data_dir);
        if let Some(relays) = &self.config.relays {
            config.mailbox_relays = relays.clone();
        }
        if let Some(retry) = self.config.retry_interval {
            config.retry_interval = retry;
        }
        let client = Client::open(config).await.map_err(|err| {
            let text = format!("{err:#}");
            if text.contains(LOCKED_BY_OTHER) {
                RpcError::new(
                    code::BUSY,
                    format!("profile {name:?} is open in another process (the app, a daemon or another one-shot run)"),
                )
            } else {
                RpcError::failed(text)
            }
        })?;
        let policy = enforcer::SharedPolicy::new(policy);
        let handle = client.handle();
        let profile = Arc::new(Profile {
            name: name.to_owned(),
            enforcer: enforcer::spawn(handle.clone(), &policy),
            handle,
            policy,
            client: Mutex::new(Some(client)),
            usage: Mutex::new(Usage {
                active: 0,
                last_used: Instant::now(),
            }),
        });
        tracing::info!(profile = name, "opened");
        *open = Some(profile.clone());
        Ok(InUse::new(profile))
    }

    /// Names of the profiles that are open now (one being opened is not listed yet).
    pub fn open_profiles(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .slots()
            .into_iter()
            .filter(|(_, slot)| slot.try_lock().is_ok_and(|open| open.is_some()))
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names
    }

    /// Profile directories under the root.
    pub fn known_profiles(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(paths::profiles_dir(&self.config.root)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| paths::check_profile_name(name).is_ok())
            .collect();
        names.sort();
        names
    }

    /// Closes (drains) every profile nobody used for `idle`.
    pub async fn close_idle(&self, idle: Duration) {
        for (name, slot) in self.slots() {
            // Being opened or used right now.
            let Ok(mut open) = slot.try_lock() else { continue };
            let is_idle = open
                .as_ref()
                .and_then(|profile| profile.idle_for())
                .is_some_and(|for_how_long| for_how_long >= idle);
            if !is_idle {
                continue;
            }
            if let Some(profile) = open.take() {
                // The slot stays locked until the core is done, so a new request for this
                // profile waits for the drain instead of racing the closing core for its lock.
                profile.close().await;
                tracing::info!(profile = name, "closed (idle)");
            }
        }
    }

    /// Closes every profile (drains them to the relays).
    pub async fn shutdown(&self) {
        for (_, slot) in self.slots() {
            let mut open = slot.lock().await;
            if let Some(profile) = open.take() {
                profile.close().await;
            }
        }
    }
}
