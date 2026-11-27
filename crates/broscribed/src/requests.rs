//! Outcomes of recent mutating requests by request ID (plan §14.5), for
//! `request.status` and for answering a repeated request without running it
//! twice. In memory only: after a daemon restart every ID is `unknown`.

use std::collections::{HashMap, VecDeque};

use broscribe_ipc::{IpcError, RequestId, RequestState};
use serde_json::Value;

/// Remembered requests; the oldest finished ones are forgotten first.
pub const DEFAULT_CAPACITY: usize = 1024;

/// What [`RequestLog::begin`] found.
#[derive(Debug, PartialEq)]
pub enum Begin {
    /// First time: run it, then call [`RequestLog::finish`].
    Fresh,
    /// Already running or finished: answer from this state.
    Seen(RequestState),
}

pub struct RequestLog {
    capacity: usize,
    states: HashMap<RequestId, RequestState>,
    order: VecDeque<RequestId>,
}

impl RequestLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            states: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn begin(&mut self, id: &RequestId) -> Begin {
        if let Some(state) = self.states.get(id) {
            return Begin::Seen(state.clone());
        }
        self.states.insert(id.clone(), RequestState::Running);
        self.order.push_back(id.clone());
        self.evict();
        Begin::Fresh
    }

    pub fn finish(&mut self, id: &RequestId, outcome: &Result<Value, IpcError>) {
        let state = match outcome {
            Ok(result) => RequestState::Done {
                result: result.clone(),
            },
            Err(error) => RequestState::Failed {
                error: error.clone(),
            },
        };
        if let Some(s) = self.states.get_mut(id) {
            *s = state;
        }
    }

    pub fn state(&self, id: &RequestId) -> RequestState {
        self.states
            .get(id)
            .cloned()
            .unwrap_or(RequestState::Unknown)
    }

    /// Forgets the oldest finished requests beyond capacity; running ones stay.
    fn evict(&mut self) {
        let mut budget = self.order.len();
        while self.states.len() > self.capacity && budget > 0 {
            budget -= 1;
            let Some(id) = self.order.pop_front() else {
                break;
            };
            if matches!(self.states.get(&id), Some(RequestState::Running)) {
                self.order.push_back(id);
            } else {
                self.states.remove(&id);
            }
        }
    }
}

impl Default for RequestLog {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id(s: &str) -> RequestId {
        RequestId::parse(s).unwrap()
    }

    #[test]
    fn repeated_ids_are_answered_from_the_log() {
        let mut log = RequestLog::new(8);
        assert_eq!(log.state(&id("a")), RequestState::Unknown);
        assert_eq!(log.begin(&id("a")), Begin::Fresh);
        assert_eq!(log.begin(&id("a")), Begin::Seen(RequestState::Running));
        log.finish(&id("a"), &Ok(json!({"job_id": "j-1"})));
        assert_eq!(
            log.state(&id("a")),
            RequestState::Done {
                result: json!({"job_id": "j-1"})
            }
        );
        assert_eq!(log.begin(&id("b")), Begin::Fresh);
        log.finish(&id("b"), &Err(IpcError::new("not_found", "m")));
        assert!(matches!(log.state(&id("b")), RequestState::Failed { .. }));
    }

    #[test]
    fn eviction_keeps_running_requests() {
        let mut log = RequestLog::new(2);
        log.begin(&id("run"));
        for n in 0..5 {
            let i = id(&format!("r{n}"));
            log.begin(&i);
            log.finish(&i, &Ok(Value::Null));
        }
        assert_eq!(log.state(&id("run")), RequestState::Running);
        assert_eq!(log.state(&id("r0")), RequestState::Unknown);
        assert!(matches!(log.state(&id("r4")), RequestState::Done { .. }));
    }
}
