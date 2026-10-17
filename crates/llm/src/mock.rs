//! A scripted provider for tests: pops one canned event list per call and
//! records the requests it receives.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::provider::{BoxFuture, ChatRequest, Provider, StreamEvent};
use crate::{Cancel, Error, Message, StopReason};

#[derive(Default)]
pub struct ScriptedProvider {
    steps: Mutex<VecDeque<Vec<StreamEvent>>>,
    observed: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    pub fn new(steps: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            observed: Mutex::new(Vec::new()),
        }
    }

    /// The `messages` slice as seen by the provider, once per call.
    pub fn observed(&self) -> Vec<Vec<Message>> {
        self.observed.lock().unwrap().clone()
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream<'a>(
        &'a self,
        request: ChatRequest<'a>,
        sink: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        _cancel: Cancel,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.observed
            .lock()
            .unwrap()
            .push(request.messages.to_vec());
        let next = self.steps.lock().unwrap().pop_front();
        Box::pin(async move {
            match next {
                Some(events) => {
                    for event in events {
                        let _ = sink.send(event);
                    }
                }
                None => {
                    let _ = sink.send(StreamEvent::Stop(StopReason::EndTurn));
                }
            }
            Ok(())
        })
    }
}
