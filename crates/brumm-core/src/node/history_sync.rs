//! Progress of the initial history sync, both sides.
//!
//! The linking device sends a [`Payload::HistoryManifest`] (messages per conversation) ahead of
//! the history chunks. The new device counts what arrives per conversation, keeps the counts in
//! the store across restarts, and reports the chats still incomplete via [`Event::HistorySync`].
//! Chunks are delivered independently, so the manifest may arrive after some of them.
//!
//! Priority: chunks are queued one conversation at a time, the most recently active first, and
//! newest messages first within it. Delivery keeps that order, so the chats on top of the list
//! and the bottom of each chat fill in first.

use anyhow::Result;

use super::Node;
use crate::{
    api::{Event, HistorySyncView},
    identity::UserKey,
    store::HistorySyncState,
    wire::{HistoryCount, HistoryItem, chunk_history},
};

/// History chunks in delivery priority: one conversation at a time, the most recently active
/// first; newest messages first within a conversation. A chunk never mixes conversations.
pub(super) fn prioritized_chunks(mut items: Vec<HistoryItem>) -> Vec<Vec<HistoryItem>> {
    let mut last_activity: Vec<(UserKey, u64)> = Vec::new();
    for item in &items {
        match last_activity
            .iter_mut()
            .find(|(contact, _)| *contact == item.contact)
        {
            Some((_, latest)) => *latest = (*latest).max(item.message.sent_at_ms),
            None => last_activity.push((item.contact, item.message.sent_at_ms)),
        }
    }
    let rank = |contact: &UserKey| {
        last_activity
            .iter()
            .find(|(known, _)| known == contact)
            .map_or(0, |(_, latest)| *latest)
    };
    items.sort_by(|a, b| {
        rank(&b.contact)
            .cmp(&rank(&a.contact))
            .then(a.contact.cmp(&b.contact))
            .then(b.message.sent_at_ms.cmp(&a.message.sent_at_ms))
    });
    let mut chunks = Vec::new();
    let mut conversation = Vec::new();
    for item in items {
        if conversation
            .last()
            .is_some_and(|last: &HistoryItem| last.contact != item.contact)
        {
            chunks.extend(chunk_history(std::mem::take(&mut conversation)));
        }
        conversation.push(item);
    }
    chunks.extend(chunk_history(conversation));
    chunks
}

/// Messages per conversation, in first-seen order.
pub(super) fn count_per_contact(items: &[HistoryItem]) -> Vec<HistoryCount> {
    let mut counts: Vec<HistoryCount> = Vec::new();
    for item in items {
        add(&mut counts, &item.contact, 1);
    }
    counts
}

fn add(counts: &mut Vec<HistoryCount>, contact: &UserKey, messages: u32) {
    match counts.iter_mut().find(|count| &count.contact == contact) {
        Some(count) => count.messages = count.messages.saturating_add(messages),
        None => counts.push(HistoryCount {
            contact: *contact,
            messages,
        }),
    }
}

/// Chats with a known total that are not complete yet. `None` until the manifest arrived.
fn pending(state: &HistorySyncState) -> Option<Vec<HistorySyncView>> {
    let expected = state.expected.as_ref()?;
    Some(
        expected
            .iter()
            .filter_map(|total| {
                let received = state
                    .received
                    .iter()
                    .find(|count| count.contact == total.contact)
                    .map_or(0, |count| count.messages);
                (received < total.messages).then_some(HistorySyncView {
                    contact: total.contact,
                    received,
                    expected: total.messages,
                })
            })
            .collect(),
    )
}

impl Node {
    pub(super) fn receive_history_manifest(&mut self, counts: Vec<HistoryCount>) -> Result<bool> {
        let mut state = self.store.history_sync()?.unwrap_or_default();
        state.expected = Some(counts);
        self.save_history_sync(state)?;
        Ok(true)
    }

    /// Counts a processed chunk (every item, also duplicates, so totals match the manifest).
    pub(super) fn note_history_chunk(&mut self, items: &[HistoryItem]) -> Result<()> {
        let mut state = self.store.history_sync()?.unwrap_or_default();
        for count in count_per_contact(items) {
            add(&mut state.received, &count.contact, count.messages);
        }
        self.save_history_sync(state)
    }

    /// Re-announces an unfinished sync after a restart.
    pub(super) fn emit_history_sync(&self) -> Result<()> {
        if let Some(chats) = self.store.history_sync()?.as_ref().and_then(pending) {
            self.events.emit(Event::HistorySync(chats));
        }
        Ok(())
    }

    /// Stores the state, or drops it once every expected chat is complete.
    fn save_history_sync(&mut self, state: HistorySyncState) -> Result<()> {
        let Some(chats) = pending(&state) else {
            return self.store.set_history_sync(Some(&state));
        };
        let done = chats.is_empty();
        self.store.set_history_sync((!done).then_some(&state))?;
        self.events.emit(Event::HistorySync(chats));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(contact: u8, messages: u32) -> HistoryCount {
        HistoryCount {
            contact: [contact; 32],
            messages,
        }
    }

    fn item(contact: u8, sent_at_ms: u64) -> HistoryItem {
        HistoryItem {
            contact: [contact; 32],
            outgoing: true,
            message: crate::wire::ChatMessage {
                id: [sent_at_ms as u8; 16],
                body: crate::wire::Body::Text("hi".into()),
                sent_at_ms,
            },
        }
    }

    #[test]
    fn chunks_put_the_most_recent_chat_and_its_newest_messages_first() {
        let chunks = prioritized_chunks(vec![item(1, 10), item(2, 5), item(1, 30), item(2, 40)]);
        let order: Vec<(u8, u64)> = chunks
            .iter()
            .flatten()
            .map(|item| (item.contact[0], item.message.sent_at_ms))
            .collect();
        assert_eq!(order, vec![(2, 40), (2, 5), (1, 30), (1, 10)]);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.iter().all(|item| item.contact == chunk[0].contact))
        );
    }

    #[test]
    fn pending_waits_for_the_manifest_and_drops_complete_chats() {
        let mut state = HistorySyncState {
            expected: None,
            received: vec![count(1, 3)],
        };
        assert_eq!(pending(&state), None);
        state.expected = Some(vec![count(1, 3), count(2, 5)]);
        assert_eq!(
            pending(&state),
            Some(vec![HistorySyncView {
                contact: [2; 32],
                received: 0,
                expected: 5,
            }])
        );
        add(&mut state.received, &[2; 32], 5);
        assert_eq!(pending(&state), Some(Vec::new()));
    }
}
