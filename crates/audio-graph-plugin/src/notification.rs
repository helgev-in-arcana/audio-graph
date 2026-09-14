// ============================================================================
//
// HUMAN REVIEW REQUIRED: THIS FILE HAS NOT BEEN REVIEWED BY A HUMAN.
//
// ============================================================================

/// Independent error conditions in the current document, each retaining only its latest message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    State,
    Graph,
    Processing,
}

#[derive(Default)]
pub(crate) struct Notifications {
    messages: [Option<String>; 3],
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
