//! Groups end to end (SPEC.md §14): real nodes over iroh (gossip + member-served catch-up), and
//! a local Nostr relay for the offline fallback.

mod common;

use std::time::Duration;

use brumm_core::{
    api::{
        Command, ConversationId, DeliveryStatus, Direction, Event, GroupId, GroupStatus, InvitationStage, Role,
    },
    identity::UserKey,
};
use brumm_core::node::GroupTiming;
use common::{TestNode, local_relay, texts};

struct Trio {
    alice: TestNode,
    bob: TestNode,
    carol: TestNode,
    carol_key: UserKey,
    group: GroupId,
}

/// Alice creates a group with her contacts Bob and Carol, who both accept.
async fn trio(prefix: &str) -> Trio {
    trio_tuned(prefix, common::TEST_GROUP_TIMING).await
}

async fn trio_tuned(prefix: &str, timing: GroupTiming) -> Trio {
    let mut alice = TestNode::spawn_tuned(&format!("{prefix}-alice"), timing);
    let mut bob = TestNode::spawn_tuned(&format!("{prefix}-bob"), timing);
    let mut carol = TestNode::spawn_tuned(&format!("{prefix}-carol"), timing);
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let carol_key = carol.create_identity("Carol").await;
    alice.befriend(&mut bob).await;
    alice.befriend(&mut carol).await;
    let group = alice.create_group("Chess club", false, vec![bob_key, carol_key]).await;
    bob.join_group(group).await;
    carol.join_group(group).await;
    alice.group_members(group, 3).await;
    bob.group_members(group, 3).await;
    carol.group_members(group, 3).await;
    Trio {
        alice,
        bob,
        carol,
        carol_key,
        group,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn members_chat_and_a_kicked_member_cannot_read_on() {
    let Trio {
        mut alice,
        mut bob,
        mut carol,
        carol_key,
        group,
    } = trio("group-kick").await;

    alice.send_group_text(group, "welcome all").await;
    bob.wait_group_texts(group, &["welcome all"]).await;
    carol.wait_group_texts(group, &["welcome all"]).await;

    bob.send_group_text(group, "hi from bob").await;
    let seen = alice.wait_group_texts(group, &["welcome all", "hi from bob"]).await;
    let from_bob = &seen[1];
    assert_eq!(from_bob.direction, Direction::Incoming);
    assert_eq!(from_bob.sender.as_ref().map(|sender| sender.display_name.as_str()), Some("Bob"));
    carol.wait_group_texts(group, &["welcome all", "hi from bob"]).await;

    // Alice kicks Carol; Carol learns it and stops; Alice rekeys after the batch window.
    alice.send(Command::RemoveMember { group, user: carol_key });
    carol
        .group_changed(|view| view.id == group && view.status == GroupStatus::Removed)
        .await;
    alice.group_members(group, 2).await;
    alice.wait_sql("SELECT count(*) FROM group_rekeys", 1).await;
    bob.wait_sql("SELECT count(*) FROM group_rekeys WHERE opened = 1", 1).await;

    alice.send_group_text(group, "carol is gone").await;
    bob.wait_group_texts(group, &["welcome all", "hi from bob", "carol is gone"]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        texts(&carol.group_messages(group).await),
        vec!["welcome all", "hi from bob"],
        "a removed member must not read the next epoch"
    );
    assert_eq!(carol.sql_number("SELECT count(*) FROM group_epochs"), 0, "keys dropped");

    // Bob leaves; Alice sees him go and stays alone.
    bob.send(Command::LeaveGroup { group });
    bob.group_changed(|view| view.id == group && view.status == GroupStatus::Left).await;
    alice.group_members(group, 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_leaving_hands_the_group_to_the_first_admin() {
    let Trio {
        mut alice,
        mut bob,
        mut carol,
        carol_key,
        group,
    } = trio("group-owner").await;
    alice.send(Command::SetRole { group, user: carol_key, admin: true });
    bob.group_changed(|view| view.id == group && view.owner.is_some()).await;
    carol
        .group_changed(|view| view.id == group && view.my_role == Some(Role::Admin))
        .await;

    alice.send(Command::LeaveGroup { group });
    let view = bob
        .group_changed(|view| view.id == group && view.owner == Some(carol_key))
        .await;
    assert_eq!(view.member_count, 2);
    carol
        .group_changed(|view| view.id == group && view.my_role == Some(Role::Owner))
        .await;
    alice.group_changed(|view| view.id == group && view.status == GroupStatus::Left).await;

    // Renaming now works for the new owner (no admin online needed for anyone else).
    carol.send(Command::RenameGroup { group, name: "Carol's club".into() });
    bob.group_changed(|view| view.id == group && view.name == "Carol's club").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_back_online_catches_up_from_other_members() {
    let Trio {
        mut alice,
        mut bob,
        carol,
        group,
        ..
    } = trio("group-catchup").await;
    let carol_offline = carol.stop().await;
    for text in ["one", "two", "three"] {
        alice.send_group_text(group, text).await;
    }
    bob.wait_group_texts(group, &["one", "two", "three"]).await;
    let _alice_offline = alice.stop().await;

    // No relay in this test: Carol can only get them from Bob.
    let mut carol = carol_offline.start();
    carol.ready().await;
    carol.wait_group_texts(group, &["one", "two", "three"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_reach_an_offline_member_through_nostr_only() {
    let (_relay, relays) = local_relay().await;
    let mut alice = TestNode::spawn_with("group-nostr-alice", relays.clone());
    let mut bob = TestNode::spawn_with("group-nostr-bob", relays);
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    alice.befriend(&mut bob).await;
    let group = alice.create_group("Letters", false, vec![bob_key]).await;
    bob.join_group(group).await;
    alice.group_members(group, 2).await;

    let bob_offline = bob.stop().await;
    let sent = alice.send_group_text(group, "via the relay").await;
    alice.status(sent, DeliveryStatus::Sent).await;
    let _alice_offline = alice.stop().await;

    let mut bob = bob_offline.start();
    bob.ready().await;
    bob.wait_group_texts(group, &["via the relay"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_own_device_gets_the_group_and_can_post() {
    let mut laptop = TestNode::spawn("group-own-laptop");
    let mut phone = TestNode::spawn("group-own-phone");
    let mut bob = TestNode::spawn("group-own-bob");
    laptop.create_identity("Alice").await;
    laptop.link(&mut phone, 2).await;
    let bob_key = bob.create_identity("Bob").await;
    laptop.befriend(&mut bob).await;
    phone.contact_added().await;

    let group = laptop.create_group("Family", false, vec![bob_key]).await;
    phone.group_changed(|view| view.id == group && view.status == GroupStatus::Active).await;
    bob.join_group(group).await;
    phone.group_members(group, 2).await;

    bob.send_group_text(group, "hello alice").await;
    laptop.wait_group_texts(group, &["hello alice"]).await;
    phone.wait_group_texts(group, &["hello alice"]).await;

    phone.send_group_text(group, "from the phone").await;
    bob.wait_group_texts(group, &["hello alice", "from the phone"]).await;
    let laptop_view = laptop.wait_group_texts(group, &["hello alice", "from the phone"]).await;
    assert_eq!(laptop_view[1].direction, Direction::Outgoing, "own device's message is ours");
}

#[tokio::test(flavor = "multi_thread")]
async fn announcement_mode_files_and_reactions() {
    let mut alice = TestNode::spawn("group-announce-alice");
    let mut bob = TestNode::spawn("group-announce-bob");
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    alice.befriend(&mut bob).await;
    let group = alice.create_group("News", true, vec![bob_key]).await;
    bob.join_group(group).await;
    alice.group_members(group, 2).await;

    bob.send(Command::SendText {
        conversation: ConversationId::Group(group),
        text: "may I?".into(),
    });
    assert!(bob.expect_error().await.contains("only admins"));

    alice.send(Command::SendFile {
        conversation: ConversationId::Group(group),
        name: "notes.txt".into(),
        data: b"group file body".to_vec(),
    });
    let messages = bob.wait_group_texts(group, &[""]).await;
    let file = &messages[0];
    assert_eq!(file.attachment.as_ref().map(|a| a.name.as_str()), Some("notes.txt"));

    bob.send(Command::React {
        conversation: ConversationId::Group(group),
        id: file.id,
        emoji: "👍".into(),
    });
    let reactions = alice
        .expect(|event| match event {
            Event::ReactionsChanged { reactions, .. } => Some(reactions.clone()),
            _ => None,
        })
        .await;
    assert_eq!(reactions.others, vec![("👍".to_owned(), 1)]);

    alice.send(Command::SetAnnouncement { group, on: false });
    bob.group_changed(|view| view.id == group && !view.announcement).await;
    bob.send_group_text(group, "now I can").await;
    alice.wait_group_texts(group, &["", "now I can"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_who_joins_later_gets_the_group_photo() {
    let mut alice = TestNode::spawn("group-photo-alice");
    let mut bob = TestNode::spawn("group-photo-bob");
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    alice.befriend(&mut bob).await;
    let group = alice.create_group("Pictures", false, Vec::new()).await;
    let photo = b"\xff\xd8 tiny jpeg stand-in".to_vec();
    alice.send(Command::SetGroupAvatar {
        group,
        image: photo.clone(),
    });
    alice.group_changed(|view| view.id == group).await;

    alice.send(Command::InviteToGroup {
        group,
        users: vec![bob_key],
    });
    bob.join_group(group).await;
    let received = bob
        .expect(|event| match event {
            Event::AvatarChanged { user, image } if *user == group => image.clone(),
            _ => None,
        })
        .await;
    assert_eq!(received, photo);
}

/// SPEC §14.10 G6: many members, live delivery to all, and the group keeps working with no admin online.
#[tokio::test(flavor = "multi_thread")]
async fn twenty_members_chat_and_go_on_without_an_admin() {
    const MEMBERS: usize = 20;
    let mut owner = TestNode::spawn("group-many-0");
    owner.create_identity("Owner").await;
    let mut members = Vec::new();
    let mut keys = Vec::new();
    for index in 1..MEMBERS {
        let mut node = TestNode::spawn(&format!("group-many-{index}"));
        keys.push(node.create_identity(&format!("Member {index}")).await);
        owner.befriend(&mut node).await;
        members.push(node);
    }
    let group = owner.create_group("Crowd", false, keys).await;
    for node in &mut members {
        node.join_group(group).await;
    }
    owner.group_members(group, MEMBERS as u32).await;

    owner.send_group_text(group, "hello crowd").await;
    for node in &mut members {
        node.wait_group_texts(group, &["hello crowd"]).await;
    }

    // The only admin goes offline; members still reach each other.
    let _owner_offline = owner.stop().await;
    let (first, rest) = members.split_first_mut().expect("members");
    first.send_group_text(group, "still here").await;
    for node in rest {
        node.wait_group_texts(group, &["hello crowd", "still here"]).await;
    }
}

/// Rekeys too large to inline: the slots go into a blob, desktops replicate the whole blob, and
/// a member whose only reachable peer is not the rekey's author still gets its slot.
#[tokio::test(flavor = "multi_thread")]
async fn a_rekey_blob_is_replicated_and_served_by_members() {
    let blob_rekeys = GroupTiming {
        inline_rekey_bytes: 0,
        ..common::TEST_GROUP_TIMING
    };
    let mut alice = TestNode::spawn_tuned("group-replica-alice", blob_rekeys);
    let mut bob = TestNode::spawn_tuned("group-replica-bob", blob_rekeys);
    let mut carol = TestNode::spawn_tuned("group-replica-carol", blob_rekeys);
    let mut dave = TestNode::spawn_tuned("group-replica-dave", blob_rekeys);
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let carol_key = carol.create_identity("Carol").await;
    let dave_key = dave.create_identity("Dave").await;
    alice.befriend(&mut bob).await;
    alice.befriend(&mut carol).await;
    alice.befriend(&mut dave).await;
    let group = alice
        .create_group("Blobs", false, vec![bob_key, carol_key, dave_key])
        .await;
    for node in [&mut bob, &mut carol, &mut dave] {
        node.join_group(group).await;
    }
    alice.group_members(group, 4).await;

    // Carol is away during the kick of Dave; Bob opens the rekey and keeps its whole blob.
    let carol_offline = carol.stop().await;
    alice.send(Command::RemoveMember { group, user: dave_key });
    alice.wait_sql("SELECT count(*) FROM group_rekeys", 1).await;
    bob.wait_sql("SELECT count(*) FROM group_rekeys WHERE opened = 1", 1).await;
    // Bob has no file or photo blobs in this group: a stored blob is the replicated rekey.
    bob.wait_sql("SELECT count(*) FROM blobs", 1).await;
    alice.send_group_text(group, "after the kick").await;
    bob.wait_group_texts(group, &["after the kick"]).await;

    // Alice leaves for good; Carol gets the rekey and her slot from Bob alone.
    let _alice_offline = alice.stop().await;
    let mut carol = carol_offline.start();
    carol.ready().await;
    carol.wait_group_texts(group, &["after the kick"]).await;
    carol
        .wait_sql("SELECT count(*) FROM group_rekeys WHERE opened = 1", 1)
        .await;
    assert_eq!(carol.sql_number("SELECT count(*) FROM blobs"), 1, "the blob came from Bob");
}

/// Checkpoints travel as blobs (a big group's does not fit a unit): members fetch and install
/// them, also one that was offline and catches up from a member after the author left.
#[tokio::test(flavor = "multi_thread")]
async fn checkpoints_are_fetched_as_blobs_also_after_being_offline() {
    const CHECKPOINT_OPS: u64 = 8;
    /// Far below the checkpoint's size, so it is fetched in several pieces.
    const BLOB_CHUNK_BYTES: u64 = 1024;
    let timing = GroupTiming {
        checkpoint_ops: CHECKPOINT_OPS,
        blob_chunk_bytes: BLOB_CHUNK_BYTES,
        ..common::TEST_GROUP_TIMING
    };
    let Trio {
        alice,
        mut bob,
        carol,
        group,
        ..
    } = trio_tuned("group-checkpoint", timing).await;
    let has_checkpoint = "SELECT count(*) FROM groups WHERE checkpoint IS NOT NULL";
    assert_eq!(bob.sql_number(has_checkpoint), 0, "the trio alone must not reach a checkpoint");

    let carol_offline = carol.stop().await;
    let last_name = format!("Name {CHECKPOINT_OPS}");
    for round in 1..=CHECKPOINT_OPS {
        alice.send(Command::RenameGroup { group, name: format!("Name {round}") });
    }
    bob.group_changed(|view| view.id == group && view.name == last_name).await;
    alice.wait_sql(has_checkpoint, 1).await;
    bob.wait_sql(has_checkpoint, 1).await;
    // Covered ops are pruned; the group goes on from the checkpoint.
    assert!(bob.sql_number("SELECT count(*) FROM group_ops") < CHECKPOINT_OPS as i64);
    let _alice_offline = alice.stop().await;

    let mut carol = carol_offline.start();
    carol.ready().await;
    carol.wait_sql(has_checkpoint, 1).await;
    // No photo, files or blob rekeys here: the only blob is the fetched checkpoint.
    assert_eq!(carol.sql_number("SELECT count(*) FROM blobs"), 1);
    let pieces = carol.sql_number("SELECT length(data) FROM blobs") as u64 / BLOB_CHUNK_BYTES;
    assert!(pieces >= 2, "the checkpoint should take several pieces, took {pieces}");
    carol.send_group_text(group, "still here").await;
    bob.wait_group_texts(group, &["still here"]).await;
}

/// Alice adds Carol while Carol is offline, then goes offline herself: the welcome waits on the
/// relay, and Carol gets the op log it points to from wherever it still is. With `bob_stays`
/// Bob serves it; otherwise nobody is online and only the chunk letters on the relay have it.
async fn join_after_the_inviter_left(prefix: &str, timing: GroupTiming, bob_stays: bool) {
    let (_relay, relays) = local_relay().await;
    let mut alice = TestNode::spawn_tuned_with(&format!("{prefix}-alice"), relays.clone(), timing);
    let mut bob = TestNode::spawn_tuned_with(&format!("{prefix}-bob"), relays.clone(), timing);
    let mut carol = TestNode::spawn_tuned_with(&format!("{prefix}-carol"), relays, timing);
    alice.create_identity("Alice").await;
    let bob_key = bob.create_identity("Bob").await;
    let carol_key = carol.create_identity("Carol").await;
    alice.befriend(&mut bob).await;
    alice.befriend(&mut carol).await;
    let group = alice.create_group("Big club", false, vec![bob_key]).await;
    bob.join_group(group).await;
    alice.group_members(group, 2).await;

    alice.send(Command::InviteToGroup { group, users: vec![carol_key] });
    carol.invitation_stage(group, InvitationStage::Open).await;
    carol.send(Command::AcceptGroupInvitation { group });
    // The invitation stays listed while Carol waits to be added.
    carol.invitation_stage(group, InvitationStage::Accepted).await;
    let carol_offline = carol.stop().await;

    alice.group_members(group, 3).await;
    bob.group_members(group, 3).await;
    // Quitting drains the welcome (and any chunk letters) to the relay.
    let _alice_offline = alice.stop().await;
    let bob = if bob_stays {
        Some(bob)
    } else {
        bob.stop().await;
        None
    };

    let mut carol = carol_offline.start();
    carol.ready().await;
    carol.invitation_stage(group, InvitationStage::Loading).await;
    carol
        .group_changed(|view| view.id == group && view.status == GroupStatus::Active && view.my_role.is_some())
        .await;
    carol.group_members(group, 3).await;
    assert_eq!(carol.sql_number("SELECT count(*) FROM group_pending_logs"), 0);
    assert_eq!(carol.sql_number("SELECT count(*) FROM group_invitations"), 0);

    if let Some(mut bob) = bob {
        bob.send_group_text(group, "welcome carol").await;
        carol.wait_group_texts(group, &["welcome carol"]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_welcome_is_fetched_from_a_member_while_the_inviter_is_offline() {
    let timing = GroupTiming {
        // As if the log were too big for the welcome and for chunk letters.
        inline_log_bytes: 0,
        nostr_log_bytes: 0,
        ..common::TEST_GROUP_TIMING
    };
    join_after_the_inviter_left("group-pointer", timing, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_group_is_joined_from_relay_chunks_with_nobody_online() {
    let timing = GroupTiming {
        inline_log_bytes: 0,
        ..common::TEST_GROUP_TIMING
    };
    join_after_the_inviter_left("group-chunks", timing, false).await;
}
