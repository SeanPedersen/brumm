//! Scale numbers for SPEC.md §14.7 / §14.11 G6, as ignored tests that print a table:
//!
//! ```sh
//! cargo test --release -p brumm-core --lib group::bench -- --ignored --nocapture --test-threads 1
//! ```
//!
//! A 10k-member group with 3 devices per member (30k devices): op log fold, checkpoint,
//! memory of the folded state, a rekey for every device, and catch-up traffic after being
//! offline for a day or a week in a busy group.

use std::time::Instant;

use iroh::SecretKey;

use super::{
    DeviceKeys, GroupId, GroupOp, MAX_GROUP_MEMBERS, OpKind, group_id, sign_consent,
    checkpoint::Checkpoint,
    keys::{build_slots, device_keypair, epoch_id, random_secret},
    oplog,
    sync::{self, Item},
};
use crate::{
    identity::{Profile, UserIdentity, tests::entry},
    wire,
};

const DEVICES_PER_MEMBER: usize = 3;
/// Messages per member per day in the "busy group" of SPEC §14.7.
const MESSAGES_PER_MEMBER_PER_DAY: usize = 10;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// Days the member-served log covers (`LOG_SERVE_WINDOW_MS`).
const LOG_DAYS: u64 = 14;
/// Ciphertext of a typical short group message incl. signature and header.
const TYPICAL_UNIT_BYTES: usize = 300;

struct Member {
    identity: UserIdentity,
    profile: Profile,
    device_keys: Vec<DeviceKeys>,
}

fn member(index: usize) -> Member {
    let identity = UserIdentity::generate();
    let devices: Vec<SecretKey> = (0..DEVICES_PER_MEMBER).map(|_| SecretKey::generate()).collect();
    let entries = devices
        .iter()
        .map(|device| entry(&identity, device.public()))
        .collect();
    let device_keys = devices
        .iter()
        .map(|device| {
            let secret = device.to_bytes();
            DeviceKeys::sign(&secret, device_keypair(&secret).1, 1)
        })
        .collect();
    Member {
        profile: Profile {
            display_name: format!("Member {index}"),
            devices: identity.sign_device_list(1, entries),
        },
        identity,
        device_keys,
    }
}

/// A group created by member 0 that added everyone else, one op at a time.
fn big_log(members: &[Member]) -> (GroupId, Vec<GroupOp>) {
    let owner = &members[0];
    let nonce = [1; 16];
    let group = group_id(&owner.identity.public(), &nonce);
    let create = GroupOp::new(
        &owner.identity,
        group,
        Vec::new(),
        1,
        OpKind::Create {
            name: "Ten thousand".into(),
            announcement: false,
            nonce,
            creator: owner.profile.clone(),
            device_keys: owner.device_keys.clone(),
        },
    );
    let mut heads = vec![create.hash()];
    let mut ops = vec![create];
    for (index, member) in members.iter().enumerate().skip(1) {
        let add = GroupOp::new(
            &owner.identity,
            group,
            heads,
            1 + index as u64,
            OpKind::Add {
                member: member.profile.clone(),
                device_keys: member.device_keys.clone(),
                rejoin: 0,
                consent: sign_consent(&member.identity, &group, 0),
            },
        );
        heads = vec![add.hash()];
        ops.push(add);
    }
    (group, ops)
}

/// Resident memory of this process in bytes (Linux; 0 elsewhere).
fn resident_bytes() -> u64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|field| field.parse().ok())
        .unwrap_or(0);
    const PAGE_BYTES: u64 = 4096;
    pages * PAGE_BYTES
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[test]
#[ignore = "scale bench, run in release with --nocapture"]
fn ten_thousand_members() {
    let started = Instant::now();
    let members: Vec<Member> = (0..MAX_GROUP_MEMBERS).map(member).collect();
    let (group, ops) = big_log(&members);
    println!("setup (keys + {} signed ops): {:.1?}", ops.len(), started.elapsed());
    let log_bytes: usize = ops.iter().map(|op| wire::encode(op).len()).sum();
    println!("op log: {:.1} MiB", mib(log_bytes as u64));

    let started = Instant::now();
    assert!(ops.iter().all(GroupOp::verify));
    println!("verify every op signature: {:.1?}", started.elapsed());
    let before = resident_bytes();
    let started = Instant::now();
    let folded = oplog::fold(group, None, &ops);
    println!("fold (permissions, order, state): {:.1?}", started.elapsed());
    assert_eq!(folded.state.members.len(), MAX_GROUP_MEMBERS);
    let state_bytes = wire::encode(&folded.state).len();
    println!(
        "folded state: {:.1} MiB encoded, process grew {:.1} MiB",
        mib(state_bytes as u64),
        mib(resident_bytes().saturating_sub(before))
    );

    let started = Instant::now();
    let checkpoint = Checkpoint::new(&members[0].identity, &folded.state, None, &ops);
    println!("checkpoint build: {:.1?}", started.elapsed());
    println!("checkpoint: {:.1} MiB", mib(wire::encode(&checkpoint).len() as u64));
    let started = Instant::now();
    assert!(checkpoint.verify() && checkpoint.matches_log(None, &ops));
    println!("checkpoint verify + refold check: {:.1?}", started.elapsed());

    // A newcomer starts from the checkpoint plus a short tail.
    let tail: Vec<GroupOp> = (0..100)
        .map(|index| {
            GroupOp::new(
                &members[0].identity,
                group,
                folded.state.heads.clone(),
                100_000 + index,
                OpKind::Rename {
                    name: format!("Name {index}"),
                },
            )
        })
        .collect();
    let started = Instant::now();
    let from_checkpoint = oplog::fold(group, Some(&checkpoint), &tail);
    println!("fold checkpoint + 100 ops: {:.1?}", started.elapsed());
    assert_eq!(from_checkpoint.state.members.len(), MAX_GROUP_MEMBERS);
}

#[test]
#[ignore = "scale bench, run in release with --nocapture"]
fn rekey_for_thirty_thousand_devices() {
    let devices = MAX_GROUP_MEMBERS * DEVICES_PER_MEMBER;
    let recipients: Vec<DeviceKeys> = (0..devices)
        .map(|_| {
            let secret = SecretKey::generate().to_bytes();
            DeviceKeys::sign(&secret, device_keypair(&secret).1, 1)
        })
        .collect();
    let group = [2; 32];
    let secret = random_secret();
    let epoch = epoch_id(&secret);
    let started = Instant::now();
    let slots = build_slots(&group, &epoch, &secret, &recipients);
    println!(
        "rekey for {devices} devices: {:.1?} on {} threads, blob {:.2} MiB",
        started.elapsed(),
        std::thread::available_parallelism().map_or(1, usize::from),
        mib(slots.len() as u64)
    );
}

/// Bytes of reconciliation (ranges and id lists, both ways) for `offline_days` behind.
fn catch_up_bytes(offline_days: u64) -> (usize, usize) {
    let per_day = MAX_GROUP_MEMBERS * MESSAGES_PER_MEMBER_PER_DAY;
    let total_days = LOG_DAYS;
    let mut theirs: Vec<Item> = (0..per_day as u64 * total_days)
        .map(|index| Item {
            sent_at_ms: index * DAY_MS / per_day as u64,
            id: *blake3::hash(&index.to_le_bytes()).as_bytes(),
        })
        .collect();
    theirs.sort();
    let cutoff = (total_days - offline_days) * DAY_MS;
    let ours: Vec<Item> = theirs.iter().filter(|item| item.sent_at_ms < cutoff).copied().collect();
    let mut bytes = 0;
    let mut lacking = 0;
    let mut ranges = sync::initiate(&ours);
    while !ranges.is_empty() {
        bytes += wire::encode(&ranges).len();
        let step = sync::respond(&theirs, &ranges);
        bytes += wire::encode(&step.reply).len() + wire::encode(&step.they_lack).len();
        lacking += step.they_lack.len();
        if step.reply.is_empty() {
            break;
        }
        let back = sync::respond(&ours, &step.reply);
        bytes += wire::encode(&back.we_lack).len();
        lacking += back.we_lack.len();
        ranges = back.reply;
    }
    (bytes, lacking)
}

#[test]
#[ignore = "scale bench, run in release with --nocapture"]
fn catch_up_after_a_day_and_a_week() {
    for days in [1, 7] {
        let started = Instant::now();
        let (bytes, lacking) = catch_up_bytes(days);
        println!(
            "offline {days} d in a busy 10k group: {lacking} missing units, reconciliation {:.2} MiB \
             ({:.1?} CPU), then ≈ {:.0} MiB of units",
            mib(bytes as u64),
            started.elapsed(),
            mib((lacking * TYPICAL_UNIT_BYTES) as u64)
        );
    }
}
