//! MQTT connections to the flespi broker.
//!
//! Every connection of the application is built here (`client`) and shares
//! the credentials in effect (`credentials`). This module owns the session of
//! a card in a PC/SC reader — the APDU bridge between the server and the card
//! — while the app connection (`app_connect`) and the rack card sessions
//! (`com_port`) reuse its client, topics and status helpers.
//!
//! Layout:
//! - `client` — building connections, TLS, connection-failure logging
//! - `credentials` — the MQTT credentials in effect
//! - `topics` — the wire contract: topics, request ids, replies, status answers
//! - `card_session` — the per-card session task (`ensure_connection`)
//! - `teardown` — ending sessions (`shutdown_connections`, `remove_connections*`)
//! - `activity` — when a card last exchanged anything (auto-update gate)

mod activity;
mod card_session;
mod client;
mod credentials;
mod teardown;
mod topics;

pub use activity::{seconds_since_card_activity, touch_card_activity};
pub(crate) use card_session::card_io;
pub use card_session::ensure_connection;
pub(crate) use client::{build_mqtt_client, log_connection_failure};
pub(crate) use credentials::{
    auth_state, known_credentials, resolved_credentials, set_session_credentials,
    take_session_credentials,
};
pub(crate) use teardown::SHUTDOWN_REASON_APP_CONNECTION_REPLACED;
pub use teardown::{remove_connections, remove_connections_all, shutdown_connections};
pub(crate) use topics::{
    card_status, request_id_from_topic, request_to_response_topic, StatusAnswers,
};
