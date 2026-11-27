//! Event fan-out to IPC subscribers (plan §14.1): each subscriber has its
//! own bounded queue. Publishing never blocks and never waits on a
//! subscriber; when a queue is full its oldest event is dropped and the
//! subscriber receives a `lagged` marker with the count before its next
//! event.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use broscribe_ipc::Event;
use tokio::sync::Notify;

/// Default per-subscriber queue length.
pub const DEFAULT_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct EventBus {
    shared: Arc<BusShared>,
}

struct BusShared {
    capacity: usize,
    subs: Mutex<Subs>,
}

#[derive(Default)]
struct Subs {
    next_id: u64,
    queues: HashMap<u64, Arc<SubQueue>>,
}

struct SubQueue {
    state: Mutex<QueueState>,
    notify: Notify,
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<Event>,
    dropped: u64,
    closed: bool,
}

/// Lock that survives a panicked holder: the protected data stays usable.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            shared: Arc::new(BusShared {
                capacity: capacity.max(1),
                subs: Mutex::new(Subs::default()),
            }),
        }
    }

    pub fn subscribe(&self) -> Subscriber {
        let queue = Arc::new(SubQueue {
            state: Mutex::new(QueueState::default()),
            notify: Notify::new(),
        });
        let mut subs = lock(&self.shared.subs);
        let id = subs.next_id;
        subs.next_id += 1;
        subs.queues.insert(id, Arc::clone(&queue));
        Subscriber {
            id,
            queue,
            bus: Arc::clone(&self.shared),
        }
    }

    /// Queues `event` for every subscriber; never blocks on a subscriber.
    /// Returns how many subscribers it was queued for.
    pub fn publish(&self, event: Event) -> usize {
        let subscribers = lock(&self.shared.subs);
        let mut queued = 0;
        for q in subscribers.queues.values() {
            {
                let mut st = lock(&q.state);
                if st.closed {
                    continue;
                }
                while st.events.len() >= self.shared.capacity {
                    st.events.pop_front();
                    st.dropped += 1;
                }
                st.events.push_back(event.clone());
            }
            q.notify.notify_one();
            queued += 1;
        }
        queued
    }

    pub fn subscriber_count(&self) -> usize {
        lock(&self.shared.subs).queues.len()
    }

    /// Ends every subscription (daemon shutdown).
    pub fn close(&self) {
        let subscribers = lock(&self.shared.subs);
        for q in subscribers.queues.values() {
            lock(&q.state).closed = true;
            q.notify.notify_one();
        }
    }
}

/// One subscriber's end; unsubscribes on drop.
pub struct Subscriber {
    id: u64,
    queue: Arc<SubQueue>,
    bus: Arc<BusShared>,
}

impl Subscriber {
    /// The next event without waiting: a `lagged` marker first when events
    /// were dropped. `None` when the queue is empty.
    pub fn try_next(&self) -> Option<Event> {
        let mut st = lock(&self.queue.state);
        if st.dropped > 0 {
            let dropped = std::mem::take(&mut st.dropped);
            return Some(Event::Lagged { dropped });
        }
        st.events.pop_front()
    }

    /// Waits for the next event; `None` once the bus is closed and drained.
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            let notified = self.queue.notify.notified();
            if let Some(e) = self.try_next() {
                return Some(e);
            }
            if lock(&self.queue.state).closed {
                return None;
            }
            notified.await;
        }
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        lock(&self.bus.subs).queues.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(n: u64) -> Event {
        Event::Lagged { dropped: 1000 + n }
    }

    #[test]
    fn slow_subscriber_lags_and_never_blocks_others() {
        let bus = EventBus::new(4);
        let slow = bus.subscribe();
        let fast = bus.subscribe();
        for n in 0..10 {
            bus.publish(ev(n));
            assert_eq!(fast.try_next(), Some(ev(n)));
        }
        // The slow one kept the 4 newest and learns that 6 were dropped.
        assert_eq!(slow.try_next(), Some(Event::Lagged { dropped: 6 }));
        for n in 6..10 {
            assert_eq!(slow.try_next(), Some(ev(n)));
        }
        assert_eq!(slow.try_next(), None);
    }

    #[test]
    fn publishing_with_a_never_reading_subscriber_stays_bounded() {
        let bus = EventBus::new(8);
        let _stuck = bus.subscribe();
        let start = std::time::Instant::now();
        for n in 0..100_000 {
            bus.publish(ev(n));
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let q = lock(&bus.shared.subs)
            .queues
            .values()
            .next()
            .cloned()
            .unwrap();
        assert_eq!(lock(&q.state).events.len(), 8);
    }

    #[test]
    fn dropping_unsubscribes() {
        let bus = EventBus::new(4);
        let s = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        drop(s);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn next_wakes_on_publish_and_ends_on_close() {
        let bus = EventBus::new(4);
        let mut sub = bus.subscribe();
        let b = bus.clone();
        let task = tokio::spawn(async move {
            b.publish(ev(1));
            b.close();
        });
        assert_eq!(sub.next().await, Some(ev(1)));
        assert_eq!(sub.next().await, None);
        task.await.unwrap();
    }
}
