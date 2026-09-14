use nice_plug::prelude::{EditorOpenHandle, EditorOpenRequest, EditorOpenStatus};

/// Independent error conditions in the current document. One message is kept per source.
#[derive(Clone, Copy)]
pub enum ErrorSource {
    State,
    Graph,
    Processing,
}

#[derive(Default)]
pub(crate) struct Notifications {
    messages: [Option<String>; 3],
    pending: bool,
    request: Option<EditorOpenHandle>,
}

impl Notifications {
    pub fn report(&mut self, source: ErrorSource, message: String) {
        let current = &mut self.messages[source as usize];
        if current.as_ref() != Some(&message) {
            *current = Some(message);
            self.pending = true;
        }
    }

    pub fn clear(&mut self, source: ErrorSource) {
        self.messages[source as usize] = None;
        if self.messages.iter().all(Option::is_none) {
            self.pending = false;
            self.cancel_request();
        }
    }

    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.messages.iter().filter_map(Option::as_deref)
    }

    pub fn take_request(&mut self, editor_open: bool) -> Option<EditorOpenRequest> {
        if editor_open {
            self.pending = false;
            self.cancel_request();
        }
        let cancelled = self
            .request
            .as_ref()
            .is_some_and(|request| request.status() == EditorOpenStatus::Cancelled);
        if !std::mem::take(&mut self.pending) && !cancelled {
            return None;
        }
        if self.request.as_ref().is_some_and(|request| {
            matches!(
                request.status(),
                EditorOpenStatus::Pending | EditorOpenStatus::Dispatching
            )
        }) {
            return None;
        }
        let request = EditorOpenRequest::default();
        self.request = Some(request.handle());
        Some(request)
    }

    fn cancel_request(&mut self) {
        if let Some(request) = self.request.take() {
            request.cancel();
        }
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        self.cancel_request();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A continuing error does not reopen a dismissed editor or retry a host rejection.
    #[test]
    fn a_continuing_error_requests_once() {
        let mut notices = Notifications::default();
        notices.report(ErrorSource::State, "unreadable".into());
        notices
            .take_request(false)
            .unwrap()
            .dispatch(|| EditorOpenStatus::Rejected);
        notices.report(ErrorSource::State, "unreadable".into());
        assert!(notices.take_request(false).is_none());
        assert_eq!(notices.messages().collect::<Vec<_>>(), ["unreadable"]);
        notices.clear(ErrorSource::State);
        notices.report(ErrorSource::State, "unreadable".into());
        assert!(notices.take_request(false).is_some());
    }

    /// New messages share an outstanding display request and remain independently visible.
    #[test]
    fn simultaneous_errors_share_the_pending_request() {
        let mut notices = Notifications::default();
        notices.report(ErrorSource::State, "state".into());
        let request = notices.take_request(false).unwrap();
        notices.report(ErrorSource::Processing, "processing".into());
        assert!(notices.take_request(false).is_none());
        assert!(request.is_pending());
        assert_eq!(notices.messages().count(), 2);
        notices.clear(ErrorSource::State);
        assert!(request.is_pending());
        notices.clear(ErrorSource::Processing);
        assert!(!request.is_pending());
    }

    /// Showing the editor manually or replacing its document cancels queued display work.
    #[test]
    fn visible_and_replaced_documents_cancel_requests() {
        let mut notices = Notifications::default();
        notices.report(ErrorSource::State, "state".into());
        let request = notices.take_request(false).unwrap();
        assert!(notices.take_request(true).is_none());
        assert!(!request.is_pending());
        assert!(notices.take_request(false).is_none());
        notices.report(ErrorSource::Graph, "graph".into());
        let request = notices.take_request(false).unwrap();
        drop(notices);
        assert!(!request.is_pending());
    }

    /// A dropped queue entry may be retried without turning a host refusal into a reopen loop.
    #[test]
    fn cancelled_delivery_retries_but_host_refusal_does_not() {
        let mut notices = Notifications::default();
        notices.report(ErrorSource::State, "state".into());
        drop(notices.take_request(false).unwrap());
        let retry = notices.take_request(false).unwrap();
        retry.dispatch(|| EditorOpenStatus::Unsupported);
        assert!(notices.take_request(false).is_none());
    }
}
