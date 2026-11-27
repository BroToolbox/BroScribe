//! Scripted [`LlmProvider`] for tests: replies are popped in order and
//! checked against the schema like real providers do.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use broscribe_core::{Error, Result};
use serde_json::Value;

use super::json::check_schema;
use super::{Completion, LlmProvider};

#[derive(Debug)]
pub struct FakeLlm {
    id: String,
    model: Option<String>,
    reported_model: Option<String>,
    replies: Mutex<VecDeque<Result<Value>>>,
    prompts: Mutex<Vec<String>>,
}

impl FakeLlm {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            model: None,
            reported_model: None,
            replies: Mutex::new(VecDeque::new()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    /// The requested model ([`LlmProvider::model`]).
    pub fn with_model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    /// The model every reply reports ([`Completion::reported_model`]).
    pub fn with_reported_model(mut self, model: &str) -> Self {
        self.reported_model = Some(model.to_string());
        self
    }

    pub fn push_reply(&self, reply: Value) {
        self.replies.lock().expect("fake lock").push_back(Ok(reply));
    }

    pub fn push_error(&self, error: Error) {
        self.replies
            .lock()
            .expect("fake lock")
            .push_back(Err(error));
    }

    /// Number of `complete_json` calls so far.
    pub fn calls(&self) -> usize {
        self.prompts.lock().expect("fake lock").len()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("fake lock").clone()
    }
}

impl LlmProvider for FakeLlm {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn complete_json(
        &self,
        prompt: &str,
        schema: &Value,
        _timeout: Duration,
    ) -> Result<Completion> {
        self.prompts
            .lock()
            .expect("fake lock")
            .push(prompt.to_string());
        let next = self.replies.lock().expect("fake lock").pop_front();
        let reply = next
            .unwrap_or_else(|| Err(Error::Inference(format!("{}: no scripted reply", self.id))))?;
        check_schema(&reply, schema).map_err(|e| Error::Schema(format!("{}: {e}", self.id)))?;
        Ok(Completion {
            value: reply,
            reported_model: self.reported_model.clone(),
        })
    }
}
