//! Provider conformance suite: every wire adapter runs the same scenarios against
//! a local fake server, so the agent loop behaves the same whichever provider
//! answers. No real network.

mod anthropic_messages;
mod faux;
mod google_generative_ai;
mod harness;
mod openai_completions;
mod openai_responses;
