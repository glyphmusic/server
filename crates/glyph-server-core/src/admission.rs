//! Bounded process-local work queue. Three account requests per guest when busy.
use std::{collections::VecDeque, sync::{Arc, Mutex}};
use futures::channel::oneshot;
use crate::ApiErr;

#[derive(Default)]
pub(crate) struct Queue(Mutex<State>);
#[derive(Default)]
struct State {
    active: usize,
    streak: usize,
    members: VecDeque<oneshot::Sender<Permit>>,
    guests: VecDeque<oneshot::Sender<Permit>>,
}
pub(crate) struct Permit(Arc<Queue>);

/// Never shared across Worker requests: a permit handed from one request wakes
/// the waiter inside the other request's I/O context, which Cloudflare reports
/// as hung requests and bills as the sender's CPU. Permits held by aborted
/// streams also never drop there. The Worker builds a Server per request, so
/// this queue is effectively unbounded on Workers; Cloudflare caps load itself.
pub(crate) fn new_queue() -> Arc<Queue> {
    Arc::default()
}
impl Queue {
    pub async fn enter(self: &Arc<Self>, member: bool) -> Result<Permit, ApiErr> {
        let rx = {
            let mut s = self.0.lock().unwrap();
            s.members.retain(|tx| !tx.is_canceled());
            s.guests.retain(|tx| !tx.is_canceled());
            if s.active < 8 {
                s.active += 1;
                return Ok(Permit(self.clone()));
            }
            if s.members.len() + s.guests.len() >= 64 {
                let mut e = ApiErr::new(503, "queue_full", "server busy; retry later");
                e.body.retry_after_s = Some(2);
                return Err(e);
            }
            let (tx, rx) = oneshot::channel();
            if member { s.members.push_back(tx); } else { s.guests.push_back(tx); }
            rx
        };
        rx.await.map_err(|_| ApiErr::unavailable("queue closed"))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut s = self.0.0.lock().unwrap();
        loop {
            let member = !s.members.is_empty() && (s.streak < 3 || s.guests.is_empty());
            let tx = if member { s.members.pop_front() } else { s.guests.pop_front() };
            let Some(tx) = tx else { s.active -= 1; return; };
            if tx.is_canceled() { continue; }
            // Release the lock before a failed handoff drops its permit.
            match tx.send(Permit(self.0.clone())) {
                Ok(()) => { if member { s.streak = (s.streak + 1).min(3); } else { s.streak = 0; } return; }
                Err(p) => { drop(s); drop(p); return; },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, FutureExt};

    #[test]
    fn priority_is_weighted_and_cancellation_releases_slots() {
        let queue = Arc::new(Queue::default());
        let mut active: Vec<_> = (0..8).map(|_| block_on(queue.enter(false)).unwrap()).collect();
        let mut guest = Box::pin(queue.enter(false));
        assert!(guest.as_mut().now_or_never().is_none());
        let mut members: Vec<_> = (0..4).map(|_| Box::pin(queue.enter(true))).collect();
        for m in &mut members { assert!(m.as_mut().now_or_never().is_none()); }
        for member in members.iter_mut().take(3) {
            drop(active.pop());
            active.push(member.as_mut().now_or_never().unwrap().unwrap());
            assert!(guest.as_mut().now_or_never().is_none());
        }
        drop(active.pop());
        let guest_permit = guest.as_mut().now_or_never().unwrap().unwrap();
        assert!(members[3].as_mut().now_or_never().is_none());
        drop(members);
        drop(active);
        drop(guest_permit);
        assert_eq!(queue.0.lock().unwrap().active, 0);
        assert!(block_on(queue.enter(false)).is_ok());
    }

    #[test]
    fn queue_is_bounded_and_canceled_waiters_are_pruned() {
        let queue = Arc::new(Queue::default());
        let _active: Vec<_> = (0..8).map(|_| block_on(queue.enter(false)).unwrap()).collect();
        let mut waiting: Vec<_> = (0..64).map(|_| Box::pin(queue.enter(false))).collect();
        for w in &mut waiting { assert!(w.as_mut().now_or_never().is_none()); }
        assert_eq!(block_on(queue.enter(true)).err().unwrap().status, 503);
        drop(waiting);
        let mut next = Box::pin(queue.enter(true));
        assert!(next.as_mut().now_or_never().is_none());
        assert_eq!(queue.0.lock().unwrap().guests.len(), 0);
    }
}
