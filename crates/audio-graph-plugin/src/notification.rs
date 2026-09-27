/// Independent error conditions in the current document, each retaining only its latest message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    State,
    Graph,
    Processing,
    /// Notes the engine lost to its fixed limits. See [`note_loss_message`].
    Notes,
    /// A document written by a newer release, or holding nodes this one could
    /// not read. See [`compatibility_message`].
    Compatibility,
}

#[derive(Default)]
pub(crate) struct Notifications {
    messages: [Option<String>; 5],
}

impl Notifications {
    pub fn report(&mut self, source: ErrorSource, message: &str) {
        let current = &mut self.messages[source as usize];
        if current.as_deref() != Some(message) {
            *current = Some(message.to_owned());
        }
    }

    pub fn clear(&mut self, source: ErrorSource) {
        self.messages[source as usize] = None;
    }

    pub fn message(&self, source: ErrorSource) -> Option<&str> {
        self.messages[source as usize].as_deref()
    }

    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.messages.iter().filter_map(Option::as_deref)
    }
}

/// What to tell the user about notes the engine lost, or `None` when it has
/// lost none.
///
/// The counts are running totals since the engine was last prepared, not per
/// block: a lost note-off leaves a note hanging long after the block that lost
/// it, so the message has to outlive that block to be of any use.
pub(crate) fn note_loss_message(dropped: u64, stolen: u64) -> Option<String> {
    let mut parts = Vec::new();
    if dropped > 0 {
        parts.push(format!(
            "{dropped} note event(s) were dropped because a note buffer was full"
        ));
    }
    if stolen > 0 {
        parts.push(format!(
            "{stolen} note(s) were cut off because more than {} were sounding at once",
            audio_graph_engine::MAX_LIVE_NOTES
        ));
    }
    (!parts.is_empty()).then(|| format!("{}.", parts.join("; ")))
}

/// What to tell the user about a document this build may not fully
/// understand, or `None` when it understands all of it.
///
/// A warning rather than a refusal: the document is open and playing, and
/// what is at stake is only what the next save writes — a field this build
/// has no place for is not written back.
pub(crate) fn compatibility_message(newer: Option<&str>, unreadable: usize) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(newer) = newer {
        parts.push(format!(
            "Saved by AudioGraph {newer}, newer than this {}. Settings this version does not know may be lost if the project is saved",
            crate::state::THIS_RELEASE
        ));
    }
    if unreadable > 0 {
        parts.push(format!(
            "{unreadable} node(s) could not be read; they are kept as saved and do not run"
        ));
    }
    (!parts.is_empty()).then(|| format!("{}.", parts.join(". ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing lost is nothing to say, and each kind of loss names itself.
    #[test]
    fn a_note_loss_message_names_only_what_was_lost() {
        assert_eq!(note_loss_message(0, 0), None);
        let dropped = note_loss_message(3, 0).unwrap();
        assert!(
            dropped.contains("3 note event(s) were dropped"),
            "{dropped}"
        );
        assert!(!dropped.contains("cut off"), "{dropped}");
        let both = note_loss_message(1, 2).unwrap();
        assert!(
            both.contains("1 note event(s)") && both.contains("2 note(s) were cut off"),
            "{both}"
        );
    }
}
