//! A group of about 1 000 members with a handful of real nodes (SPEC.md §14.11).
//!
//! Running 1 000 real nodes is out of reach for one machine, so the owner's database gets ~1 000
//! extra members as signed `Add` ops (generated identities, one device each, never online)
//! while she is stopped. After her restart every size-dependent path runs for real: a
//! checkpoint blob of about a megabyte, a rekey for ~1 000 devices as a blob, welcomes by
//! pointer with a log too big for Nostr chunks, and members that must find the few online
//! devices among a thousand offline ones.

mod common;

use std::time::{Duration, Instant};

use brumm_core::{
    api::{Command, GroupId},
    group::{
        DeviceKeys, GroupOp, OpKind, keys::device_keypair, oplog, sign_consent,
    },
    identity::{DeviceEntry, Profile, UserIdentity},
    mailbox, wire,
};
use brumm_core::node::GroupTiming;
use common::{Stopped, TestNode};
use iroh::SecretKey;
use rusqlite::params;

/// Generated members on top of the real ones.
const SYNTHETIC_MEMBERS: usize = 1_000;
/// Smaller than the ~0.5 MiB checkpoint, so newcomers fetch it in several pieces.
const BLOB_CHUNK_BYTES: u64 = 128 * 1024;
/// Mailbox relay of the generated members: nothing listens there.
const NO_RELAY: &str = "ws://127.0.0.1:9";

/// A generated member's part of its `Add`: profile, group key and consent.
struct Joiner {
    member: Profile,
    device_keys: Vec<DeviceKeys>,
    consent: ed25519_dalek::Signature,
}

/// One generated member: a signed device list with one device, its group key, and consent.
fn synthetic_member(group: GroupId, at_ms: u64, index: usize) -> Joiner {
    let identity = UserIdentity::generate();
    let device = SecretKey::generate();
    let device_secret = device.to_bytes();
    let entry = DeviceEntry {
        cert: identity.certify_device(device.public(), [3; 32], at_ms),
        fallback_key: [4; 32],
        mailbox: mailbox::address(&device_secret, vec![NO_RELAY.to_owned()]),
        label: "Linux".into(),
    };
    let member = Profile {
        display_name: format!("Member {index}"),
        devices: identity.sign_device_list(1, vec![entry]),
    };
    let device_keys = vec![DeviceKeys::sign(&device_secret, device_keypair(&device_secret).1, at_ms)];
    let consent = sign_consent(&identity, &group, 0);
    Joiner {
        member,
        device_keys,
        consent,
    }
}

/// The members' own signatures in parallel (the slow part in a debug build).
fn synthetic_members(group: GroupId, at_ms: u64) -> Vec<Joiner> {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let per_thread = SYNTHETIC_MEMBERS.div_ceil(threads);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                scope.spawn(move || {
                    let first = thread * per_thread;
                    let last = (first + per_thread).min(SYNTHETIC_MEMBERS);
                    (first..last).map(|index| synthetic_member(group, at_ms, index)).collect::<Vec<_>>()
                })
            })
            .collect();
        workers.into_iter().flat_map(|worker| worker.join().unwrap()).collect()
    })
}

/// Appends `SYNTHETIC_MEMBERS` Adds signed by the owner to her stopped database.
fn add_synthetic_members(owner_node: &Stopped, owner: &UserIdentity, group: GroupId) {
    let db = owner_node.db();
    let ops: Vec<GroupOp> = db
        .prepare("SELECT op FROM group_ops WHERE group_id = ?1")
        .unwrap()
        .query_map(params![group.as_slice()], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|bytes| wire::decode(&bytes.unwrap()).unwrap())
        .collect();
    let mut heads = oplog::fold(group, None, &ops).state.heads;
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let tx = db.unchecked_transaction().unwrap();
    for (index, joiner) in synthetic_members(group, start_ms).into_iter().enumerate() {
        let kind = OpKind::Add {
            member: joiner.member,
            device_keys: joiner.device_keys,
            rejoin: 0,
            consent: joiner.consent,
        };
        let op = GroupOp::new(owner, group, heads, start_ms + index as u64, kind);
        tx.execute(
            "INSERT INTO group_ops (group_id, hash, op) VALUES (?1, ?2, ?3)",
            params![group.as_slice(), op.hash().as_slice(), wire::encode(&op)],
        )
        .unwrap();
        heads = vec![op.hash()];
    }
    tx.commit().unwrap();
}

fn step(label: &str, started: &mut Instant) {
    eprintln!("[large group] {label}: {:.1?}", started.elapsed());
    *started = Instant::now();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thousand_member_group_with_a_few_online() {
    let mut clock = Instant::now();
    let timing = GroupTiming {
        blob_chunk_bytes: BLOB_CHUNK_BYTES,
        ..common::TEST_GROUP_TIMING
    };
    let mut alice = TestNode::spawn_tuned("large-alice", timing);
    let mut bob = TestNode::spawn_tuned("large-bob", timing);
    let mut carol = TestNode::spawn_tuned("large-carol", timing);
    let mut dave = TestNode::spawn_tuned("large-dave", timing);
    let (_, phrase) = alice.create_identity_with_phrase("Alice").await;
    let owner = UserIdentity::from_phrase(&phrase).unwrap();
    let bob_key = bob.create_identity("Bob").await;
    let carol_key = carol.create_identity("Carol").await;
    let dave_key = dave.create_identity("Dave").await;
    alice.befriend(&mut bob).await;
    alice.befriend(&mut carol).await;
    alice.befriend(&mut dave).await;
    let group = alice.create_group("Thousand", false, Vec::new()).await;
    let alice_offline = alice.stop().await;
    add_synthetic_members(&alice_offline, &owner, group);
    step("setup + 1000 signed Adds", &mut clock);

    // Restart: 1001 members, a checkpoint and a rekey for ~1000 devices become due.
    let mut alice = alice_offline.start();
    alice.ready().await;
    alice.wait_sql("SELECT count(*) FROM groups WHERE checkpoint IS NOT NULL", 1).await;
    alice.wait_sql("SELECT count(*) FROM blobs", 2).await;
    step("owner restart: checkpoint + rekey blobs", &mut clock);
    let checkpoint_bytes = alice.sql_number("SELECT length(checkpoint) FROM groups") as u64;
    eprintln!(
        "[large group] checkpoint {} KiB, rekey blob {} KiB",
        checkpoint_bytes / 1024,
        alice.sql_number("SELECT min(length(data)) FROM blobs") / 1024
    );
    assert!(checkpoint_bytes > 2 * BLOB_CHUNK_BYTES, "the checkpoint must take several pieces");

    // Three real members join by pointer (log far above the Nostr chunk limit).
    alice.send(Command::InviteToGroup {
        group,
        users: vec![bob_key, carol_key, dave_key],
    });
    bob.join_group(group).await;
    step("bob joined", &mut clock);
    carol.join_group(group).await;
    dave.join_group(group).await;
    let real = SYNTHETIC_MEMBERS as u32 + 4;
    step("carol + dave joined", &mut clock);
    alice.group_members(group, real).await;
    bob.group_members(group, real).await;

    alice.send_group_text(group, "hello thousand").await;
    for node in [&mut bob, &mut carol, &mut dave] {
        node.wait_group_texts(group, &["hello thousand"]).await;
    }
    bob.send_group_text(group, "hi from bob").await;
    for node in [&mut alice, &mut carol, &mut dave] {
        node.wait_group_texts(group, &["hello thousand", "hi from bob"]).await;
    }
    step("live chat among the online four", &mut clock);

    // A kick: a rekey for ~1000 devices as a blob; the others fetch their slot or the blob.
    alice.send(Command::RemoveMember { group, user: dave_key });
    bob.group_members(group, real - 1).await;
    alice.wait_sql("SELECT count(*) FROM group_rekeys", 2).await;
    alice.send_group_text(group, "dave is out").await;
    bob.wait_group_texts(group, &["hello thousand", "hi from bob", "dave is out"]).await;
    carol.wait_group_texts(group, &["hello thousand", "hi from bob", "dave is out"]).await;
    // Dave is still online, but the new epoch was never sealed for him.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let dave_sees = common::texts(&dave.group_messages(group).await);
    assert_eq!(dave_sees, ["hello thousand", "hi from bob"]);
    step("kick + rekey blob for ~1000 devices", &mut clock);

    // Catch-up: Carol was away; only Bob is online among a thousand members.
    let carol_offline = carol.stop().await;
    for text in ["one", "two", "three"] {
        alice.send_group_text(group, text).await;
    }
    bob.wait_group_texts(group, &["hello thousand", "hi from bob", "dave is out", "one", "two", "three"])
        .await;
    let _alice_offline = alice.stop().await;
    let mut carol = carol_offline.start();
    carol.ready().await;
    carol
        .wait_group_texts(group, &["hello thousand", "hi from bob", "dave is out", "one", "two", "three"])
        .await;
    step("catch-up from the one online member", &mut clock);
}
