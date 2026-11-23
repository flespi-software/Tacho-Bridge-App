//! The `ManagedCard` handle of a card in a PC/SC reader: the APDU exchange
//! with its retry rule, the reset after a session, the protocol switch, and
//! the ICCID checks that catch a card swapped behind a live session.
//!
//! Every PC/SC call on the handle runs on the blocking pool and is tracked as
//! in flight on its reader (see `reader_io`) until it returns.

use std::error::Error as StdError;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{debug, error, info, warn};
use once_cell::sync::OnceCell;
use pcsc::{Card, Context, Disposition, Protocols, Scope, ShareMode};
use tauri::async_runtime::Mutex;

use super::monitor::request_rescan;
use super::protocol::protocol_to_str;
use super::reader_io::ReaderIoGuard;
use super::{DynError, DynResult, ProcessingCard, TASK_POOL};

/// Failure of one APDU exchange, classified for the retry decision in
/// `send_apdu` and reported to the MQTT bridge as a typed transport error —
/// distinct from any status word the card itself can return (a genuine `6F00`
/// reply is an `Ok`, not an error).
#[derive(Debug)]
pub enum ApduError {
    /// The request hex could not be decoded — nothing was ever sent, and a
    /// retry with the same bytes cannot succeed either.
    BadRequest(String),
    /// PCSC transmit failed; carries the typed error so the retry decision can
    /// tell a dead handle from a failure of an already-delivered command.
    Pcsc(pcsc::Error),
    /// The blocking transmit task died (panicked, or was cancelled before it
    /// started) without producing a result.
    TaskDied(String),
    /// The PCSC handle could not be recreated after a retriable failure.
    RecreateFailed(String),
    /// An earlier card operation is still running on the blocking pool: its
    /// caller timed out, but a PC/SC call cannot be interrupted. The command
    /// is not queued behind it — once the reader recovers it would run late,
    /// after the server already re-sent it, and execute twice.
    Busy,
}

impl std::fmt::Display for ApduError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApduError::BadRequest(e) => write!(f, "APDU decode error: {}", e),
            ApduError::Pcsc(e) => write!(f, "PCSC transmit error: {}", e),
            ApduError::TaskDied(e) => write!(f, "APDU transmit task died: {}", e),
            ApduError::RecreateFailed(e) => write!(f, "card handle recreate failed: {}", e),
            ApduError::Busy => write!(f, "previous card operation still in progress"),
        }
    }
}

impl StdError for ApduError {}

impl ApduError {
    /// True when the failure proves (or makes it overwhelmingly likely) that
    /// the command was NOT delivered to the card — the only situation where
    /// recreating the handle and re-sending the same APDU is safe. Replaying a
    /// command the card may already have executed would run a stateful APDU
    /// twice and corrupt the authentication state.
    ///
    /// `RemovedCard` is deliberately not here: the card that held the session
    /// is gone, and the one now in the reader may be a different card with the
    /// same ATR (see `send_apdu`).
    pub(crate) fn is_retriable(&self) -> bool {
        match self {
            // Nothing was sent, but a retry with the same bytes fails the same way.
            ApduError::BadRequest(_) => false,
            ApduError::Pcsc(e) => matches!(
                e,
                pcsc::Error::InvalidHandle
                    | pcsc::Error::ResetCard
                    | pcsc::Error::ReaderUnavailable
                    | pcsc::Error::NoService
                    | pcsc::Error::ServiceStopped
                    // the card had no power / never answered / was absent:
                    // nothing was executed
                    | pcsc::Error::UnpoweredCard
                    | pcsc::Error::UnresponsiveCard
                    | pcsc::Error::NoSmartcard
            ),
            // A cancelled blocking task never started (spawn_blocking closures
            // cannot be interrupted once running), and the only panic sites sit
            // before the transmit call — the command did not reach the card.
            ApduError::TaskDied(_) => true,
            ApduError::RecreateFailed(_) => false,
            // Not sent, but a recreate would queue behind the stuck operation.
            ApduError::Busy => false,
        }
    }
}

/// The PCSC handle shared by a `ManagedCard`, its clones, and the blocking
/// closures running PC/SC calls on it.
struct CardCell {
    /// `None` only between disconnecting a handle and connecting its
    /// replacement (see `replace_handle`), or when that reconnect failed: a
    /// transmit then fails with `InvalidHandle`, which is retriable and
    /// recreates the handle.
    slot: Mutex<Option<Card>>,
    reader_name: Arc<CStr>,
    /// Set by `release`: the handle is closed with LeaveCard instead of the
    /// ResetCard a dropped handle has always meant.
    leave_on_drop: AtomicBool,
}

impl Drop for CardCell {
    /// The last owner may be a card task ending (or aborted) on an async
    /// worker, and `SCardDisconnect` — pcsc's own `Drop` included — is a
    /// synchronous call that can block on a wedged reader. The handle is
    /// therefore closed on the blocking pool, with the disposition a plain
    /// drop had: ResetCard, unless the handle was released.
    fn drop(&mut self) {
        let Some(card) = self.slot.get_mut().take() else {
            return;
        };
        let disposition = if self.leave_on_drop.load(Ordering::SeqCst) {
            Disposition::LeaveCard
        } else {
            Disposition::ResetCard
        };
        let io = ReaderIoGuard::new(&self.reader_name);
        let reader_name = Arc::clone(&self.reader_name);
        tauri::async_runtime::spawn_blocking(move || {
            let _io = io;
            if let Err((_, e)) = card.disconnect(disposition) {
                // the returned handle is dropped (and the card reset) here anyway
                debug!(
                    "Closing the card handle for reader {} failed: {}",
                    reader_name.to_string_lossy(),
                    e
                );
            }
        });
    }
}

/// True when a card operation failed because an earlier one still holds the
/// handle (see `ApduError::Busy`).
fn is_busy(e: &DynError) -> bool {
    matches!(e.downcast_ref::<ApduError>(), Some(ApduError::Busy))
}

/// Owns a shared PCSC card handle, caches its ICCID, and manages APDU
/// exchanges and reconnection using the selected protocol.
pub struct ManagedCard {
    inner: Arc<CardCell>,
    reader_name: Arc<CStr>,
    /// The T protocol the handle is connected with (see `switch_protocol`).
    pub(super) protocol: Protocols,
    pub iccid: OnceCell<String>,
    /// The `TASK_POOL` entry this card serves (see `bind_session`).
    session_id: Option<u64>,
}

impl ManagedCard {
    pub fn new(reader_name: &CStr, protocol: Protocols) -> DynResult<Self> {
        debug!(
            "ManagedCard::new() called. Reader: '{}', Protocol: {:?}",
            reader_name.to_string_lossy(),
            protocol
        );

        let card = Self::create_card(reader_name, protocol)?;
        debug!(
            "Card successfully created for reader: '{}'",
            reader_name.to_string_lossy()
        );

        let reader_name: Arc<CStr> = Arc::from(reader_name.to_owned());
        Ok(Self {
            inner: Arc::new(CardCell {
                slot: Mutex::new(Some(card)),
                reader_name: Arc::clone(&reader_name),
                leave_on_drop: AtomicBool::new(false),
            }),
            reader_name,
            protocol,
            iccid: OnceCell::new(),
            session_id: None,
        })
    }

    /// Ties the card to the pool entry of its session: a swap detected later
    /// ends exactly that entry, never a successor registered in between under
    /// the same card number and reader.
    pub fn bind_session(&mut self, session_id: u64) {
        self.session_id = Some(session_id);
    }

    fn create_card(reader_name: &CStr, protocol: Protocols) -> DynResult<Card> {
        let ctx = Context::establish(Scope::User)?;
        let card = ctx.connect(reader_name, ShareMode::Shared, protocol)?;

        Ok(card)
    }

    /// `SCardReconnect` on the current handle, run on the blocking pool: a
    /// cold reset takes hundreds of ms and can block for much longer on a
    /// wedged reader, which must not stall an async worker (and with it the
    /// MQTT event loops of every other card).
    ///
    /// Fails fast with `ApduError::Busy` while an earlier operation still
    /// holds the handle, like `apdu_transmit`: waiting for the lock would park
    /// one more blocking-pool thread per attempt behind a call that may never
    /// return, and a reset queued that way runs late, in the middle of
    /// whatever session the card serves by then.
    async fn reconnect_handle(
        &self,
        protocol: Protocols,
        disposition: Disposition,
    ) -> DynResult<()> {
        let inner = Arc::clone(&self.inner);
        let io = ReaderIoGuard::new(&self.reader_name);
        tauri::async_runtime::spawn_blocking(move || -> DynResult<()> {
            let _io = io;
            let Ok(mut slot) = inner.slot.try_lock() else {
                return Err(Box::new(ApduError::Busy));
            };
            let card = slot.as_mut().ok_or("no card handle")?;
            card.reconnect(ShareMode::Shared, protocol, disposition)?;
            Ok(())
        })
        .await
        .map_err(|e| format!("reconnect task died: {e}"))?
    }

    /// Replaces the card handle with a freshly connected one, on the blocking
    /// pool. The old handle is disconnected with `old_disposition` — dropping
    /// it would always reset the card (pcsc's `Drop` hard-codes ResetCard).
    ///
    /// A reset through the old handle flags every other handle to the card as
    /// reset, so with `ResetCard` the old handle goes first and the new one is
    /// connected afterwards: in the other order the fresh handle's first
    /// transmit would fail with `ResetCard`. Any other disposition connects
    /// first and keeps the old handle if that fails.
    ///
    /// Fails fast with `ApduError::Busy` while the handle is held (see
    /// `reconnect_handle`).
    async fn replace_handle(
        &self,
        protocol: Protocols,
        old_disposition: Disposition,
    ) -> DynResult<()> {
        let inner = Arc::clone(&self.inner);
        let reader_name = Arc::clone(&self.reader_name);
        let io = ReaderIoGuard::new(&self.reader_name);
        tauri::async_runtime::spawn_blocking(move || -> DynResult<()> {
            let _io = io;
            let disconnect = |old: Card| {
                if let Err((_, e)) = old.disconnect(old_disposition) {
                    // the handle is dropped (and the card reset) on the way out anyway
                    debug!(
                        "Disconnecting the old card handle for reader {} failed: {}",
                        reader_name.to_string_lossy(),
                        e
                    );
                }
            };
            let Ok(mut slot) = inner.slot.try_lock() else {
                return Err(Box::new(ApduError::Busy));
            };
            if old_disposition == Disposition::ResetCard {
                if let Some(old) = slot.take() {
                    disconnect(old);
                }
                *slot = Some(Self::create_card(&reader_name, protocol)?);
            } else {
                let new_card = Self::create_card(&reader_name, protocol)?;
                if let Some(old) = slot.replace(new_card) {
                    disconnect(old);
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| format!("recreate task died: {e}"))?
    }

    /// Switches the card to another T protocol: reconnects the underlying PCSC
    /// card with it and remembers it for future reconnects/recreates. `protocol`
    /// is a plain copied field, so clones keep the old value - valid call sites
    /// are before the ManagedCard is cloned into the task pool, and inside the
    /// card's own MQTT task, which is the sole clone touching the card afterwards.
    pub async fn switch_protocol(&mut self, protocol: Protocols) {
        if protocol == self.protocol {
            return;
        }

        info!(
            "Switching card protocol to {} for reader: {}",
            protocol_to_str(protocol),
            self.reader_name.to_string_lossy()
        );

        match self
            .reconnect_handle(protocol, Disposition::ResetCard)
            .await
        {
            Ok(()) => self.protocol = protocol,
            // The handle is untouched and still held by a stuck operation: a
            // recreate would only fail the same way.
            Err(e) if is_busy(&e) => warn!(
                "Failed to switch card protocol to {} for reader {}: {}. Keeping {}.",
                protocol_to_str(protocol),
                self.reader_name.to_string_lossy(),
                e,
                protocol_to_str(self.protocol)
            ),
            Err(e) => {
                warn!(
                    "Failed to switch card protocol to {} for reader {}: {}. Will try to recreate.",
                    protocol_to_str(protocol),
                    self.reader_name.to_string_lossy(),
                    e
                );
                match self.replace_handle(protocol, Disposition::ResetCard).await {
                    Ok(()) => self.protocol = protocol,
                    Err(e) => {
                        error!(
                            "Failed to recreate card with protocol {} for reader {}: {}. Keeping {}.",
                            protocol_to_str(protocol),
                            self.reader_name.to_string_lossy(),
                            e,
                            protocol_to_str(self.protocol)
                        );
                        // The old handle was already disconnected: bring one
                        // back with the protocol we keep.
                        if let Err(e) = self.recreate(Disposition::LeaveCard).await {
                            error!(
                                "Failed to reopen card for reader {}: {}",
                                self.reader_name.to_string_lossy(),
                                e
                            );
                        }
                    }
                }
            }
        }
    }

    /// Current T protocol in the config string form ("T0"/"T1").
    pub fn protocol_str(&self) -> &'static str {
        protocol_to_str(self.protocol)
    }

    /// Closes the handle without resetting the card. pcsc's `Drop` resets it,
    /// which wipes the state another handle to the same card relies on — a
    /// live session's, when this one was only a probe, or another
    /// application's. Only for a handle that has not been cloned.
    ///
    /// When an operation that outlived its timeout still holds the handle, it
    /// cannot be closed now: it is marked instead, and closed with LeaveCard
    /// on the blocking pool once that operation lets go (see `CardCell`).
    pub fn release(self) {
        self.inner.leave_on_drop.store(true, Ordering::SeqCst);
        let Ok(mut slot) = self.inner.slot.try_lock() else {
            return;
        };
        if let Some(card) = slot.take() {
            if let Err((_, e)) = card.disconnect(Disposition::LeaveCard) {
                debug!(
                    "Releasing the card handle for reader {} failed: {}",
                    self.reader_name.to_string_lossy(),
                    e
                );
            }
        }
    }

    pub async fn reconnect(&self) {
        debug!(
            "Attempting to reconnect card for reader: {}",
            self.reader_name.to_string_lossy()
        );

        // Reconnect with the same protocol the card was opened with (stored or
        // ATR-derived) - ANY would let PCSC renegotiate and silently change it.
        match self
            .reconnect_handle(self.protocol, Disposition::ResetCard)
            .await
        {
            Ok(()) => {
                debug!(
                    "Card reconnected successfully for reader: {}",
                    self.reader_name.to_string_lossy()
                );
            }
            // A stuck operation holds the handle: a recreate would fail the
            // same way. The card check that follows every reset hits the
            // same Busy, which leaves the card marked dirty (see
            // `ensure_same_card`).
            Err(e) if is_busy(&e) => warn!(
                "Failed to reconnect card: {} for reader: {}",
                e,
                self.reader_name.to_string_lossy()
            ),
            Err(e) => {
                warn!(
                    "Failed to reconnect card: {} for reader: {}. Will try to recreate.",
                    e,
                    self.reader_name.to_string_lossy()
                );

                // The reset the failed reconnect owed is done by disposing of
                // the old handle with ResetCard.
                if let Err(e) = self.recreate(Disposition::ResetCard).await {
                    error!(
                        "Failed to recreate card after reconnect failure for reader {}: {}",
                        self.reader_name.to_string_lossy(),
                        e
                    );
                }
            }
        }
    }

    /// Replaces the card handle with a freshly connected one; the old handle
    /// is disconnected with `old_disposition` (see `replace_handle`).
    pub async fn recreate(&self, old_disposition: Disposition) -> DynResult<()> {
        self.replace_handle(self.protocol, old_disposition).await?;

        info!(
            "Successfully recreated card object for reader: {}",
            self.reader_name.to_string_lossy()
        );

        Ok(())
    }

    pub async fn apdu_transmit(&self, apdu_hex: &str) -> Result<String, ApduError> {
        let apdu = match hex::decode(apdu_hex) {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to decode APDU '{}': {}", apdu_hex, err);
                return Err(ApduError::BadRequest(err.to_string()));
            }
        };

        let card = Arc::clone(&self.inner);
        let io = ReaderIoGuard::new(&self.reader_name);

        // The send/response pair is logged by send_apdu() with the client_id
        // header; this layer only logs failures. The buffer is sized for
        // extended-length responses: a RAPDU past 258 bytes is legitimate
        // (chained/extended reads) and must not fail the exchange. It is not
        // tried small first: PC/SC reports a too-small buffer only after the
        // card executed the command, and re-sending it would run a stateful
        // APDU twice.
        let response = tauri::async_runtime::spawn_blocking(move || {
            let _io = io;
            let mut rapdu_buf = vec![0u8; pcsc::MAX_BUFFER_SIZE_EXTENDED];

            // The card's operations run one at a time from its session task,
            // so a held lock means one of them outlived its caller's timeout.
            let Ok(locked) = card.slot.try_lock() else {
                return Err(ApduError::Busy);
            };
            let Some(locked) = locked.as_ref() else {
                // a previous reset-recreate could not reconnect: retriable
                return Err(ApduError::Pcsc(pcsc::Error::InvalidHandle));
            };

            match locked.transmit(&apdu, &mut rapdu_buf) {
                Ok(response) => Ok(hex::encode(response)),
                Err(err) => {
                    error!("APDU transmit failed: {}", err);
                    // Preserved as the typed pcsc error so send_apdu can tell a
                    // dead handle from an error of an already-delivered command.
                    Err(ApduError::Pcsc(err))
                }
            }
        })
        .await;

        match response {
            Ok(result) => result,
            // The blocking task itself died: keep this distinct from a pcsc
            // failure so the retry decision does not lump it into the
            // "may have reached the card" bucket (see ApduError::is_retriable).
            Err(join_err) => Err(ApduError::TaskDied(join_err.to_string())),
        }
    }

    /// Runs one APDU against the card, recreating the PCSC handle and retrying
    /// once when the failure proves the command never reached the card.
    ///
    /// `Ok` carries the card's actual reply — any status word, including a
    /// genuine `6F00`. `Err` means the exchange failed at the transport level
    /// and no card reply exists: the caller substitutes `SW_TECHNICAL_PROBLEM`
    /// on the wire and must keep that reply out of the idempotency cache so the
    /// server's retry reaches the card again (same rule the rack path applies
    /// via `SerialExchange::is_ok()`).
    pub async fn send_apdu(&self, apdu_hex: &str, client_id: &str) -> Result<String, ApduError> {
        log::trace!("{} Sending APDU command: {}", client_id, apdu_hex);

        // First attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                log::trace!("{} APDU response: {:?}", client_id, response);
                return Ok(response);
            }
            Err(err) => {
                // Recreate-and-retry is only safe when the failure proves the
                // command never reached the card (dead handle / reader / PCSC
                // service, or a transmit task that died before sending). For
                // anything else the card may already have executed it —
                // recreating resets the card (destroying the SM auth state)
                // and replaying a stateful APDU would run it twice.
                if matches!(err, ApduError::Pcsc(pcsc::Error::RemovedCard)) {
                    self.handle_removed_card(client_id).await;
                    return Err(err);
                }
                if !err.is_retriable() {
                    error!(
                        "{} Failed to send APDU: {}. Not retried: the command may have reached the card.",
                        client_id, err
                    );
                    return Err(err);
                }
                error!(
                    "{} Failed to send APDU: {}. Command not delivered - attempting to recreate card...",
                    client_id, err
                );
            }
        }

        // recreate attempt; the retry must reach the card in its current
        // state, so the old handle leaves it as it is
        if let Err(e) = self.recreate(Disposition::LeaveCard).await {
            error!(
                "{} Failed to recreate card after APDU failure: {}",
                client_id, e
            );
            return Err(ApduError::RecreateFailed(e.to_string()));
        }

        // Second attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                log::trace!(
                    "{} APDU response (after recreate): {:?}",
                    client_id,
                    response
                );
                Ok(response)
            }
            Err(retry_err) => {
                error!(
                    "{} Retry failed: could not send APDU after recreate: {}",
                    client_id, retry_err
                );
                Err(retry_err)
            }
        }
    }

    /// The card behind the session was removed. Its security state is gone,
    /// so the APDU is not replayed; what is left to decide is whether the card
    /// now in the reader is the same one. PC/SC folds a fast swap into one
    /// PRESENT event, and cards of one product share an ATR, so the monitor can
    /// miss a swap entirely — this session would then keep running against the
    /// other card, answering the VU with it and saving its data under this
    /// card's config entry. A different (or unreadable) card ends the session
    /// and asks the monitor to rescan, which registers whatever is inserted.
    async fn handle_removed_card(&self, client_id: &str) {
        let reader = self.reader_name.to_string_lossy().into_owned();
        // LeaveCard: the removed card has nothing to reset, and a reset here
        // would flag the fresh handle.
        let current = match self.recreate(Disposition::LeaveCard).await {
            Ok(()) => self.read_iccid().await.ok(),
            Err(e) => {
                warn!(
                    "{} Card removed from reader {} and not reopened: {}",
                    client_id, reader, e
                );
                None
            }
        };
        if self.continue_if_same(client_id, current) {
            info!(
                "{} Card in reader {} was reinserted (same ICCID); the session continues after a reset",
                client_id, reader
            );
        }
    }

    /// Re-reads the ICCID after the card was reset or its handle replaced. A
    /// reset of a swapped card succeeds silently — and a failed reconnect falls
    /// back to a fresh handle to whatever card is inserted — so without this
    /// check the session would carry on against the other card (see
    /// `handle_removed_card`). Returns whether the card was verified.
    ///
    /// A check that could not run because an earlier operation still holds
    /// the handle proves nothing about the card: it returns `false` without
    /// ending the session — the same outcome as a check that timed out, which
    /// leaves the card dirty so the next session start resets and checks it.
    pub async fn ensure_same_card(&self, client_id: &str) -> bool {
        match self.read_iccid().await {
            Err(e) if is_busy(&e) => {
                warn!(
                    "{} Card in reader {} not verified: {}",
                    client_id,
                    self.reader_name.to_string_lossy(),
                    e
                );
                false
            }
            current => self.continue_if_same(client_id, current.ok()),
        }
    }

    /// Ends the session and asks for a rescan unless `current` is this
    /// session's card. Returns whether the session continues.
    fn continue_if_same(&self, client_id: &str, current: Option<String>) -> bool {
        let expected = self.iccid.get();
        if current.is_some() && current.as_ref() == expected {
            return true;
        }
        let reader = self.reader_name.to_string_lossy().into_owned();
        warn!(
            "{} Card in reader {} changed (ICCID {:?} → {:?}): ending the session and rescanning",
            client_id, reader, expected, current
        );
        // Detached: callers run under a timeout that would cancel a cleanup
        // half-way and leave the stale entry behind, and this very task is
        // among those being shut down.
        let client_id = client_id.to_owned();
        let session_id = self.session_id;
        tauri::async_runtime::spawn(async move {
            let stale: Vec<ProcessingCard> = {
                let mut pool = TASK_POOL.lock().await;
                let (stale, keep) =
                    std::mem::take(&mut *pool)
                        .into_iter()
                        .partition(|c| match session_id {
                            Some(id) => c.session_id == id,
                            None => {
                                c.client_id == client_id
                                    && c.reader_name.as_deref() == Some(reader.as_str())
                            }
                        });
                *pool = keep;
                stale
            };
            if !stale.is_empty() {
                crate::mqtt::shutdown_connections(stale, "card_swapped").await;
            }
            // After the cleanup: an entry left in the pool would make the
            // rescan ignore the inserted card, and the reset closing the old
            // handle would fail the ICCID read of a probe racing it.
            request_rescan();
        });
        false
    }

    /// Returns the card ICCID using lazy caching.
    /// On first call, reads it from the card; subsequent calls return the cached value.
    pub async fn get_iccid(&self) -> DynResult<String> {
        if let Some(cached) = self.iccid.get() {
            debug!(
                "Returning cached ICCID for reader {}: {}",
                self.reader_name.to_string_lossy(),
                cached
            );
            return Ok(cached.clone());
        }

        let iccid = self.read_iccid().await?;
        // Save ICCID, not got earlier
        let _ = self.iccid.set(iccid.clone());
        Ok(iccid)
    }

    /// Reads the ICCID from the card itself, bypassing the cache.
    async fn read_iccid(&self) -> DynResult<String> {
        debug!(
            "read_iccid() started for reader: {}",
            self.reader_name.to_string_lossy()
        );

        // SELECT EF_ICC (FID 0002) under MF
        let select_result = self.apdu_transmit("00A4020C020002").await?;

        if !select_result.ends_with("9000") {
            // Do NOT proceed to READ BINARY: on SW != 9000 the current EF is
            // unchanged, so READ would return bytes from whatever was previously
            // selected (e.g. EF_Identification from a prior auth session),
            // producing a garbage ICCID that happens to be part of cardNumber.
            return Err(format!("SELECT EF_ICC failed with status: {}", select_result).into());
        }

        // READ BINARY: 8 bytes of cardExtendedSerialNumber at offset 1 (skipping clockStop)
        let read_response = self.apdu_transmit("00B0000108").await?;

        let Some(hex_data) = read_response.strip_suffix("9000") else {
            return Err(format!(
                "READ BINARY EF_ICC returned unexpected status: {}",
                read_response
            )
            .into());
        };

        let bytes =
            hex::decode(hex_data).map_err(|e| format!("Failed to decode ICCID hex: {}", e))?;

        if bytes.len() != 8 {
            return Err(format!(
                "EF_ICC READ BINARY returned {} bytes, expected 8",
                bytes.len()
            )
            .into());
        }

        let iccid = bytes
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<String>();

        debug!("Final ICCID: {}", iccid);

        Ok(iccid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_retriable_only_for_undelivered_commands() {
        let pcsc = |e| ApduError::Pcsc(e);
        for e in [
            pcsc::Error::InvalidHandle,
            pcsc::Error::ResetCard,
            pcsc::Error::ReaderUnavailable,
            pcsc::Error::NoService,
            pcsc::Error::ServiceStopped,
            pcsc::Error::UnpoweredCard,
            pcsc::Error::UnresponsiveCard,
            pcsc::Error::NoSmartcard,
        ] {
            assert!(pcsc(e).is_retriable(), "{e:?} must be retried");
        }
        // The command may have reached the card, or the card is a different one.
        for e in [
            pcsc::Error::RemovedCard,
            pcsc::Error::Timeout,
            pcsc::Error::CommError,
            pcsc::Error::UnknownError,
        ] {
            assert!(!pcsc(e).is_retriable(), "{e:?} must not be retried");
        }
        assert!(!ApduError::BadRequest(String::new()).is_retriable());
        assert!(!ApduError::RecreateFailed(String::new()).is_retriable());
        assert!(ApduError::TaskDied(String::new()).is_retriable());
    }

    #[test]
    fn busy_is_recognised_through_the_boxed_error() {
        let busy: DynError = Box::new(ApduError::Busy);
        assert!(is_busy(&busy));
        let other: DynError = Box::new(ApduError::Pcsc(pcsc::Error::Timeout));
        assert!(!is_busy(&other));
        let plain: DynError = "no card handle".into();
        assert!(!is_busy(&plain));
    }
}
