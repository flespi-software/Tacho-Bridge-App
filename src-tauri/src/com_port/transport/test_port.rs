//! Test support: a scripted stand-in for a rack's serial port, the placeholder
//! byte strings the tests exchange over it, and the envelopes they send.

use std::sync::Arc;
use std::time::Duration;

use serialport::SerialPort;

use super::envelope::{PollSpec, SerialEnvelope};

// ── Scripted port for the transport tests ──
// Placeholder bytes — only the SHAPE of the exchange is TBA's business (a command,
// the predicted first reply, an outcome frame, the predicted poll reply, and a
// reply that is neither). The timing shape covers the case this code has to get right: a
// reply that arrives promptly, followed by a result the device pushes on its own about one
// idle-window later, i.e. exactly into the gap between two reads. Absolute values are
// scaled up here so a loaded CI host cannot turn the scheduling into a coin flip.

pub(super) const CMD: &str = "C0DE01";
pub(super) const ACCEPTED: &str = "AC01";
pub(super) const RESULT: &str = "1234ABCD";
pub(super) const POLL_CMD: &str = "5701";
pub(super) const BUSY: &str = "B501";
pub(super) const IDLE_STATUS: &str = "1D01";

pub(super) const T_IDLE: Duration = Duration::from_millis(100);
pub(super) const T_DEADLINE: Duration = Duration::from_millis(1000);
const T_INTERVAL: Duration = Duration::from_millis(100);

/// What the scripted port has been told to answer, and what it holds now.
pub(super) struct PortState {
    /// written hex -> replies to schedule, each at its own delay from that write
    rules: Vec<(String, Vec<(Duration, String)>)>,
    pub(super) scheduled: Vec<(std::time::Instant, Vec<u8>)>,
    inbox: std::collections::VecDeque<u8>,
    timeout: Duration,
    /// Every frame written to the port, in order — shared so a test can still read it
    /// after the port has been boxed into a `dyn SerialPort`.
    writes: Arc<std::sync::Mutex<Vec<String>>>,
}

impl PortState {
    /// Moves every reply whose time has come into the readable buffer.
    fn pump(&mut self) {
        let now = std::time::Instant::now();
        let mut still = Vec::new();
        for (at, bytes) in std::mem::take(&mut self.scheduled) {
            if at <= now {
                self.inbox.extend(bytes);
            } else {
                still.push((at, bytes));
            }
        }
        self.scheduled = still;
    }

    fn next_due(&self) -> Option<std::time::Instant> {
        self.scheduled.iter().map(|(at, _)| *at).min()
    }
}

/// Stand-in for the rack's serial port. Replies are delivered on the real clock, so the
/// production timing rules (first-byte budget, line-silence bound, poll interval) run
/// exactly as they do on hardware. State lives behind a mutex because `bytes_to_read`
/// takes `&self` yet has to advance the schedule.
pub(super) struct ScriptedPort {
    pub(super) state: std::sync::Mutex<PortState>,
    pub(super) writes: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ScriptedPort {
    pub(super) fn new(rules: &[(&str, &[(u64, &str)])]) -> Self {
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        Self {
            writes: writes.clone(),
            state: std::sync::Mutex::new(PortState {
                rules: rules
                    .iter()
                    .map(|(cmd, replies)| {
                        (
                            cmd.to_string(),
                            replies
                                .iter()
                                .map(|(ms, hex)| (Duration::from_millis(*ms), hex.to_string()))
                                .collect(),
                        )
                    })
                    .collect(),
                scheduled: Vec::new(),
                inbox: std::collections::VecDeque::new(),
                timeout: Duration::from_millis(0),
                writes,
            }),
        }
    }

    /// Bytes already sitting in the buffer when the exchange starts — a leftover of an
    /// operation that is already over, or a result pushed while nobody was reading.
    pub(super) fn with_pending(self, hex: &str) -> Self {
        self.state
            .lock()
            .unwrap()
            .inbox
            .extend(hex::decode(hex).unwrap());
        self
    }
}

impl std::io::Read for ScriptedPort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let deadline = std::time::Instant::now() + self.state.lock().unwrap().timeout;
        loop {
            let wake = {
                let mut st = self.state.lock().unwrap();
                st.pump();
                if !st.inbox.is_empty() {
                    let n = buf.len().min(st.inbox.len());
                    for slot in buf.iter_mut().take(n) {
                        *slot = st.inbox.pop_front().unwrap();
                    }
                    return Ok(n);
                }
                st.next_due().map(|d| d.min(deadline)).unwrap_or(deadline)
            };
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "scripted",
                ));
            }
            if wake > now {
                std::thread::sleep(wake - now);
            }
        }
    }
}

impl std::io::Write for ScriptedPort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut st = self.state.lock().unwrap();
        let written = hex::encode_upper(buf);
        let now = std::time::Instant::now();
        let replies: Vec<(Duration, String)> = st
            .rules
            .iter()
            .find(|(cmd, _)| *cmd == written)
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        for (delay, hex) in replies {
            st.scheduled.push((now + delay, hex::decode(&hex).unwrap()));
        }
        st.writes.lock().unwrap().push(written);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SerialPort for ScriptedPort {
    fn name(&self) -> Option<String> {
        Some("scripted".into())
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(115_200)
    }
    fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
        Ok(serialport::DataBits::Eight)
    }
    fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
        Ok(serialport::FlowControl::None)
    }
    fn parity(&self) -> serialport::Result<serialport::Parity> {
        Ok(serialport::Parity::None)
    }
    fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
        Ok(serialport::StopBits::One)
    }
    fn timeout(&self) -> Duration {
        self.state.lock().unwrap().timeout
    }
    fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
        Ok(())
    }
    fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_flow_control(&mut self, _: serialport::FlowControl) -> serialport::Result<()> {
        Ok(())
    }
    fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
        Ok(())
    }
    fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.state.lock().unwrap().timeout = timeout;
        Ok(())
    }
    fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
        Ok(())
    }
    fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
        Ok(())
    }
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn bytes_to_read(&self) -> serialport::Result<u32> {
        let mut st = self.state.lock().unwrap();
        st.pump();
        Ok(st.inbox.len() as u32)
    }
    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(0)
    }
    fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
        self.state.lock().unwrap().inbox.clear();
        Ok(())
    }
    fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
        Err(serialport::Error::new(
            serialport::ErrorKind::Unknown,
            "not cloneable",
        ))
    }
    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }
    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}

/// Envelope of a command that is accepted first and finished by the poll loop.
pub(super) fn two_phase_envelope() -> SerialEnvelope {
    SerialEnvelope {
        cmd_hex: CMD.into(),
        expect_hex: Some(ACCEPTED.into()),
        idle: T_IDLE,
        deadline: T_DEADLINE,
        finish: None,
        poll: Some(PollSpec {
            cmd_hex: POLL_CMD.into(),
            while_hex: BUSY.into(),
            interval: T_INTERVAL,
            deadline: Duration::from_millis(2000),
        }),
    }
}

/// One-shot envelope (no poll spec) for the given command.
pub(super) fn one_shot_envelope(cmd: &str) -> SerialEnvelope {
    SerialEnvelope {
        cmd_hex: cmd.into(),
        expect_hex: None,
        idle: T_IDLE,
        deadline: T_DEADLINE,
        poll: None,
        finish: None,
    }
}
