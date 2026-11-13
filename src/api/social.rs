//! Social REST calls (FEATURES_NOTES Part 2): relationships, DMs, inbox, profiles (threads and
//! forums are in `api/threads.rs`). Called from `backend::handle_social`, which turns results
//! into `SocialEvent`s / `ActionFailed`. Every call goes through `ApiClient::send` / `send_with`
//! (token only to the API host, rate limiter, 401 handling, captcha -> "do it in the official
//! app"); none is retried on 4xx. Routes, bodies and headers are in the doc comments and in
//! docs/SOCIAL_DESIGN.md.
//!
//! OWNERSHIP: REST engineer (docs/SOCIAL_DESIGN.md). The signatures are the contract.
//! Relationships, DMs, inbox, profiles and notes live in `social/people.rs`.

mod people;
