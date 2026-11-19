use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A one-slot mailbox for renders where only the newest request matters.
///
/// Every request takes a ticket. Renders run one at a time, and a request whose
/// ticket was superseded while it waited for its turn gives up without rendering,
/// so a burst of requests renders the one running now and the newest one, never
/// the ones in between.
pub struct LatestWins {
    latest: AtomicU64,
    running: Mutex<()>,
}

pub struct Ticket(u64);

impl LatestWins {
    pub const fn new() -> Self {
        Self {
            latest: AtomicU64::new(0),
            running: Mutex::new(()),
        }
    }

    /// Takes a ticket for a new request, superseding every earlier one.
    pub fn ticket(&self) -> Ticket {
        Ticket(self.latest.fetch_add(1, Ordering::SeqCst) + 1)
    }

    pub fn is_current(&self, ticket: &Ticket) -> bool {
        self.latest.load(Ordering::SeqCst) == ticket.0
    }

    /// Runs `work` once no other render is running, unless a newer ticket was taken
    /// by then. Returns `None` for a superseded request.
    pub fn run<R>(&self, ticket: &Ticket, work: impl FnOnce() -> R) -> Option<R> {
        if !self.is_current(ticket) {
            return None;
        }
        let _running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if !self.is_current(ticket) {
            return None;
        }
        Some(work())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    #[test]
    fn a_lone_request_runs() {
        let mailbox = LatestWins::new();
        let ticket = mailbox.ticket();
        assert_eq!(mailbox.run(&ticket, || 7), Some(7));
    }

    #[test]
    fn a_superseded_request_does_not_run() {
        let mailbox = LatestWins::new();
        let old = mailbox.ticket();
        let new = mailbox.ticket();
        assert!(!mailbox.is_current(&old));
        assert_eq!(mailbox.run(&old, || ()), None);
        assert_eq!(mailbox.run(&new, || ()), Some(()));
    }

    #[test]
    fn only_the_newest_waiting_request_runs_after_the_current_render() {
        let mailbox = Arc::new(LatestWins::new());
        let renders = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let first = mailbox.ticket();
        let running = {
            let mailbox = Arc::clone(&mailbox);
            let renders = Arc::clone(&renders);
            std::thread::spawn(move || {
                mailbox.run(&first, || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    renders.fetch_add(1, Ordering::SeqCst);
                })
            })
        };
        started_rx.recv().unwrap();

        // three requests arrive while the first render is still running
        let waiting: Vec<_> = (0..3)
            .map(|_| {
                let ticket = mailbox.ticket();
                let mailbox = Arc::clone(&mailbox);
                let renders = Arc::clone(&renders);
                std::thread::spawn(move || {
                    mailbox.run(&ticket, || {
                        renders.fetch_add(1, Ordering::SeqCst);
                    })
                })
            })
            .collect();

        release_tx.send(()).unwrap();
        assert!(running.join().unwrap().is_some());
        let ran = waiting
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(Option::is_some)
            .count();
        assert_eq!(ran, 1);
        assert_eq!(renders.load(Ordering::SeqCst), 2);
    }
}
