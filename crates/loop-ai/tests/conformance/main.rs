//! Provider conformance suite: every wire adapter runs the same scenarios against
//! a local fake server, so the agent loop behaves the same whichever provider
//! answers. No real network.

mod faux;
mod harness;
mod openai_completions;
