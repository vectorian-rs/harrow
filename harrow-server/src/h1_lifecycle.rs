use std::fmt;

/// Shared connection phases for Harrow's HTTP/1 lifecycle.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Free,
    Headers,
    Body,
    Dispatching,
    Writing,
    Closed,
}

/// Pending socket operation for the current connection state.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PendingIo {
    None,
    Recv,
    Write,
}

/// Abstract lifecycle events that are backend-agnostic.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Accept,
    HeadersNeedMore,
    HeadersParsed {
        has_body: bool,
    },
    BodyNeedMore,
    BodyDone,
    EarlyResponse,
    DispatchDone,
    WriteProgress,
    WriteDone {
        keep_alive: bool,
        buffered_next_request: bool,
    },
    ProtocolError,
    BodyLimitExceeded,
    Timeout,
    IoError,
    Shutdown,
    ClosedCqe,
}

/// What the backend should do next after a lifecycle transition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    None,
    Dispatch,
    ArmRecv,
    ArmWrite,
    ReuseConnection { buffered_next_request: bool },
    Close,
    AwaitClosedCqe,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Machine {
    pub state: ConnectionState,
    pub pending_io: PendingIo,
    pub shutdown: bool,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub const fn new() -> Self {
        Self {
            state: ConnectionState::Free,
            pending_io: PendingIo::None,
            shutdown: false,
        }
    }

    pub fn apply(&mut self, event: Event) -> Result<Action, InvalidTransition> {
        use ConnectionState::*;
        use Event::*;
        use PendingIo::*;

        let previous = *self;
        let action = match (self.state, self.pending_io, event) {
            (Free, PendingIo::None, Accept) if !self.shutdown => {
                self.state = Headers;
                self.pending_io = Recv;
                Action::ArmRecv
            }

            (Headers, Recv, HeadersNeedMore) => Action::ArmRecv,
            (Headers, Recv, HeadersParsed { has_body: false }) => {
                self.state = Dispatching;
                self.pending_io = PendingIo::None;
                Action::Dispatch
            }
            (Headers, Recv, HeadersParsed { has_body: true }) => {
                self.state = Body;
                self.pending_io = Recv;
                Action::ArmRecv
            }

            (Body, Recv, BodyNeedMore) => Action::ArmRecv,
            (Body, Recv, BodyDone) => {
                self.state = Dispatching;
                self.pending_io = PendingIo::None;
                Action::Dispatch
            }
            (Body, Recv, EarlyResponse) => {
                self.state = Writing;
                self.pending_io = Write;
                Action::ArmWrite
            }

            (Dispatching, PendingIo::None, DispatchDone) => {
                self.state = Writing;
                self.pending_io = Write;
                Action::ArmWrite
            }

            (Writing, Write, WriteProgress) => Action::ArmWrite,
            (
                Writing,
                Write,
                WriteDone {
                    keep_alive: true,
                    buffered_next_request,
                },
            ) => {
                self.state = Headers;
                self.pending_io = Recv;
                Action::ReuseConnection {
                    buffered_next_request,
                }
            }
            (
                Writing,
                Write,
                WriteDone {
                    keep_alive: false, ..
                },
            ) => {
                self.state = Free;
                self.pending_io = PendingIo::None;
                Action::Close
            }

            // An error response can be decided while reading headers (parse
            // error, Content-Length over the limit) or the body (bad chunk
            // framing, body over the limit, body read timeout).
            (Headers | Body, Recv, ProtocolError | BodyLimitExceeded) => {
                self.state = Writing;
                self.pending_io = Write;
                Action::ArmWrite
            }

            // The server stops waiting on a pending read or write: header or
            // body read timeout, connection lifetime, end of the drain. The
            // lifetime also bounds a response the peer reads too slowly (or
            // that stalls), so a write can be abandoned too. The slot is
            // kept until the abandoned operation completes.
            (Headers | Body | Writing, Recv | Write, Timeout) => {
                self.state = Closed;
                Action::AwaitClosedCqe
            }
            (Dispatching, PendingIo::None, Timeout) => {
                self.state = Free;
                self.pending_io = PendingIo::None;
                Action::Close
            }
            (Headers | Body | Writing, Recv | Write, IoError) => {
                self.state = Free;
                self.pending_io = PendingIo::None;
                Action::Close
            }
            (Closed, Recv | Write, ClosedCqe) => {
                self.state = Free;
                self.pending_io = PendingIo::None;
                Action::Close
            }
            (_, _, Shutdown) => {
                self.shutdown = true;
                Action::None
            }
            _ => {
                return Err(InvalidTransition {
                    state: previous.state,
                    pending_io: previous.pending_io,
                    shutdown: previous.shutdown,
                    event,
                });
            }
        };

        Ok(action)
    }

    pub fn invariant_holds(&self) -> bool {
        use ConnectionState::*;
        use PendingIo::*;

        match self.state {
            Free => self.pending_io == None,
            Closed => matches!(self.pending_io, Recv | Write),
            Dispatching => self.pending_io == None,
            Headers => self.pending_io == Recv,
            Body => self.pending_io == Recv,
            Writing => self.pending_io == Write,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct InvalidTransition {
    pub state: ConnectionState,
    pub pending_io: PendingIo,
    pub shutdown: bool,
    pub event: Event,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid H1 lifecycle transition: state={:?} pending_io={:?} shutdown={} event={:?}",
            self.state, self.pending_io, self.shutdown, self.event
        )
    }
}

impl std::error::Error for InvalidTransition {}

/// Record a connection's lifecycle events and replay them against [`Machine`].
///
/// Backends call [`trace::Trace::record`] at each lifecycle step. With the
/// `lifecycle-trace` feature, a finished connection's events are kept for
/// [`trace::take_recorded`]; without it, `Trace` is empty and recording is a
/// no-op. Tests replay the recorded traces with [`trace::replay`], so a
/// backend that drifts from the model fails a test instead of silently
/// diverging.
pub mod trace {
    use super::{ConnectionState, Event, InvalidTransition, Machine};

    /// One connection's lifecycle events, in order.
    #[derive(Debug, Default)]
    pub struct Trace {
        #[cfg(feature = "lifecycle-trace")]
        events: Vec<Event>,
    }

    impl Trace {
        pub fn new() -> Self {
            Self::default()
        }

        #[inline]
        pub fn record(&mut self, event: Event) {
            #[cfg(feature = "lifecycle-trace")]
            self.events.push(event);
            #[cfg(not(feature = "lifecycle-trace"))]
            let _ = event;
        }
    }

    #[cfg(feature = "lifecycle-trace")]
    static RECORDED: std::sync::Mutex<Vec<Vec<Event>>> = std::sync::Mutex::new(Vec::new());

    #[cfg(feature = "lifecycle-trace")]
    impl Drop for Trace {
        fn drop(&mut self) {
            let events = std::mem::take(&mut self.events);
            if let Ok(mut recorded) = RECORDED.lock() {
                recorded.push(events);
            }
        }
    }

    /// Take every trace recorded since the last call (one per finished
    /// connection, in completion order).
    #[cfg(feature = "lifecycle-trace")]
    pub fn take_recorded() -> Vec<Vec<Event>> {
        RECORDED
            .lock()
            .map(|mut recorded| std::mem::take(&mut *recorded))
            .unwrap_or_default()
    }

    /// Why a trace does not fit the model.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ReplayError {
        /// The event at `step` is not a valid transition.
        Invalid {
            step: usize,
            error: InvalidTransition,
        },
        /// Every event fit, but the connection did not end `Free`.
        Unfinished { state: ConnectionState },
    }

    impl std::fmt::Display for ReplayError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Invalid { step, error } => write!(f, "step {step}: {error}"),
                Self::Unfinished { state } => {
                    write!(f, "trace ended in state {state:?}, expected Free")
                }
            }
        }
    }

    /// Replay one connection's events from a fresh [`Machine`]. Succeeds if
    /// every event is a valid transition, every intermediate state satisfies
    /// the machine's invariant, and the connection ends `Free`.
    pub fn replay(events: &[Event]) -> Result<Machine, ReplayError> {
        let mut machine = Machine::new();
        for (step, &event) in events.iter().enumerate() {
            machine
                .apply(event)
                .map_err(|error| ReplayError::Invalid { step, error })?;
            debug_assert!(machine.invariant_holds(), "invariant broken at step {step}");
        }
        if machine.state != ConnectionState::Free {
            return Err(ReplayError::Unfinished {
                state: machine.state,
            });
        }
        Ok(machine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_accepts_a_keep_alive_then_close_trace() {
        use Event::*;
        let events = [
            Accept,
            HeadersNeedMore,
            HeadersParsed { has_body: true },
            BodyNeedMore,
            BodyDone,
            DispatchDone,
            WriteDone {
                keep_alive: true,
                buffered_next_request: false,
            },
            HeadersParsed { has_body: false },
            DispatchDone,
            WriteDone {
                keep_alive: false,
                buffered_next_request: false,
            },
        ];
        assert!(trace::replay(&events).is_ok());
    }

    #[test]
    fn replay_reports_the_failing_step_and_unfinished_traces() {
        use Event::*;
        let invalid = [Accept, DispatchDone];
        assert!(matches!(
            trace::replay(&invalid),
            Err(trace::ReplayError::Invalid { step: 1, .. })
        ));
        let unfinished = [Accept, HeadersParsed { has_body: false }];
        assert!(matches!(
            trace::replay(&unfinished),
            Err(trace::ReplayError::Unfinished {
                state: ConnectionState::Dispatching
            })
        ));
    }

    #[test]
    fn error_response_while_reading_the_body() {
        use Event::*;
        let events = [
            Accept,
            HeadersParsed { has_body: true },
            BodyLimitExceeded,
            WriteDone {
                keep_alive: false,
                buffered_next_request: false,
            },
        ];
        assert!(trace::replay(&events).is_ok());
    }

    #[derive(Clone, Debug)]
    struct ScriptCase {
        seed: u64,
        events: Vec<Event>,
    }

    #[derive(Clone, Debug)]
    struct ScriptFailure {
        seed: u64,
        step: usize,
        event: Event,
        state: Machine,
    }

    impl fmt::Display for ScriptFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "seed={} step={} event={:?} state={:?}",
                self.seed, self.step, self.event, self.state
            )
        }
    }

    #[derive(Clone)]
    struct ScriptRng(u64);

    impl ScriptRng {
        fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<T: Copy>(&mut self, values: &[T]) -> T {
            let idx = (self.next() as usize) % values.len();
            values[idx]
        }
    }

    fn allowed_events(machine: &Machine) -> &'static [Event] {
        use Event::*;
        match (machine.state, machine.pending_io) {
            (ConnectionState::Free, PendingIo::None) => &[Accept, Shutdown],
            (ConnectionState::Headers, PendingIo::Recv) => &[
                HeadersNeedMore,
                HeadersParsed { has_body: false },
                HeadersParsed { has_body: true },
                ProtocolError,
                BodyLimitExceeded,
                Timeout,
                IoError,
                Shutdown,
            ],
            (ConnectionState::Body, PendingIo::Recv) => &[
                BodyNeedMore,
                BodyDone,
                EarlyResponse,
                Timeout,
                IoError,
                Shutdown,
            ],
            (ConnectionState::Dispatching, PendingIo::None) => &[DispatchDone, Timeout, Shutdown],
            (ConnectionState::Writing, PendingIo::Write) => &[
                WriteProgress,
                WriteDone {
                    keep_alive: true,
                    buffered_next_request: false,
                },
                WriteDone {
                    keep_alive: true,
                    buffered_next_request: true,
                },
                WriteDone {
                    keep_alive: false,
                    buffered_next_request: false,
                },
                IoError,
                Timeout,
                Shutdown,
            ],
            (ConnectionState::Closed, PendingIo::Recv)
            | (ConnectionState::Closed, PendingIo::Write) => &[ClosedCqe, Shutdown],
            _ => &[Shutdown],
        }
    }

    fn generate_script(seed: u64, steps: usize) -> ScriptCase {
        let mut rng = ScriptRng::new(seed);
        let mut machine = Machine::new();
        let mut events = Vec::with_capacity(steps);

        for _ in 0..steps {
            let choices = allowed_events(&machine);
            let event = rng.pick(choices);
            events.push(event);
            let _ = machine.apply(event);
            if machine.shutdown && machine.state == ConnectionState::Free {
                break;
            }
        }

        ScriptCase { seed, events }
    }

    fn run_script(case: &ScriptCase) -> Result<Machine, ScriptFailure> {
        let mut machine = Machine::new();
        for (step, event) in case.events.iter().copied().enumerate() {
            machine.apply(event).map_err(|_| ScriptFailure {
                seed: case.seed,
                step,
                event,
                state: machine,
            })?;
            if !machine.invariant_holds() {
                return Err(ScriptFailure {
                    seed: case.seed,
                    step,
                    event,
                    state: machine,
                });
            }
        }
        Ok(machine)
    }

    #[test]
    fn early_response_moves_from_body_to_write() {
        let mut machine = Machine::new();
        assert_eq!(machine.apply(Event::Accept), Ok(Action::ArmRecv));
        assert_eq!(
            machine.apply(Event::HeadersParsed { has_body: true }),
            Ok(Action::ArmRecv)
        );
        assert_eq!(machine.apply(Event::EarlyResponse), Ok(Action::ArmWrite));
        assert_eq!(machine.state, ConnectionState::Writing);
        assert_eq!(machine.pending_io, PendingIo::Write);
    }

    #[test]
    fn keep_alive_reset_reports_buffered_request_bytes() {
        let mut machine = Machine::new();
        assert_eq!(machine.apply(Event::Accept), Ok(Action::ArmRecv));
        assert_eq!(
            machine.apply(Event::HeadersParsed { has_body: false }),
            Ok(Action::Dispatch)
        );
        assert_eq!(machine.apply(Event::DispatchDone), Ok(Action::ArmWrite));
        assert_eq!(
            machine.apply(Event::WriteDone {
                keep_alive: true,
                buffered_next_request: true,
            }),
            Ok(Action::ReuseConnection {
                buffered_next_request: true
            })
        );
        assert_eq!(machine.state, ConnectionState::Headers);
        assert_eq!(machine.pending_io, PendingIo::Recv);
    }

    #[test]
    fn timeout_with_pending_io_waits_for_closed_cqe() {
        let mut machine = Machine::new();
        assert_eq!(machine.apply(Event::Accept), Ok(Action::ArmRecv));
        assert_eq!(machine.apply(Event::Timeout), Ok(Action::AwaitClosedCqe));
        assert_eq!(machine.state, ConnectionState::Closed);
        assert_eq!(machine.pending_io, PendingIo::Recv);
        assert_eq!(machine.apply(Event::ClosedCqe), Ok(Action::Close));
        assert_eq!(machine.state, ConnectionState::Free);
        assert_eq!(machine.pending_io, PendingIo::None);
    }

    #[test]
    fn timeout_while_writing_waits_for_the_abandoned_write() {
        use Event::*;
        let events = [
            Accept,
            HeadersParsed { has_body: false },
            DispatchDone,
            WriteProgress,
            Timeout,
            ClosedCqe,
        ];
        let mut machine = Machine::new();
        for event in &events[..4] {
            machine.apply(*event).unwrap();
        }
        assert_eq!(machine.apply(Timeout), Ok(Action::AwaitClosedCqe));
        assert_eq!(machine.state, ConnectionState::Closed);
        assert_eq!(machine.pending_io, PendingIo::Write);
        assert!(trace::replay(&events).is_ok());
    }

    #[test]
    fn seeded_scripts_are_replayable_and_preserve_invariants() {
        let seeds = [1_u64, 2, 3, 4, 99, 512, 1024, 65_537];
        for seed in seeds {
            let case = generate_script(seed, 64);
            let replay = case.clone();
            let end_state = run_script(&case).unwrap_or_else(|err| panic!("{err}"));
            let replay_state = run_script(&replay).unwrap_or_else(|err| panic!("{err}"));
            assert_eq!(end_state, replay_state, "seed={seed}");
        }
    }

    #[test]
    fn shutdown_scripts_can_drain_to_free() {
        let case = ScriptCase {
            seed: 7,
            events: vec![
                Event::Accept,
                Event::HeadersParsed { has_body: false },
                Event::DispatchDone,
                Event::Shutdown,
                Event::WriteDone {
                    keep_alive: false,
                    buffered_next_request: false,
                },
            ],
        };

        let end_state = run_script(&case).unwrap();
        assert!(end_state.shutdown);
        assert_eq!(end_state.state, ConnectionState::Free);
        assert_eq!(end_state.pending_io, PendingIo::None);
    }
}
