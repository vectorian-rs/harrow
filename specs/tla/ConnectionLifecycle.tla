---- MODULE ConnectionLifecycle ----
(*
 * TLA+ specification for the harrow HTTP connection state machine.
 *
 * Models N connections managed by a single-threaded worker with a
 * slab allocator. Verifies safety and liveness properties.
 *
 * Run: tla specs/tla/ConnectionLifecycle.tla -c MaxConns=3 --check-liveness
 *)

EXTENDS Integers, FiniteSets, Sequences

CONSTANT MaxConns       \* Maximum concurrent connections (e.g., 3 for model checking)

VARIABLES conns, pending_io, shutdown, free_slots

vars == <<conns, pending_io, shutdown, free_slots>>

ConnStates == {"Free", "Headers", "Body", "Dispatching", "Writing", "Closed"}
IoTypes == {"None", "Recv", "Write"}
SlabIndices == 1..MaxConns

(* ---- Type invariant ---- *)

TypeOK ==
    /\ conns \in [SlabIndices -> ConnStates]
    /\ pending_io \in [SlabIndices -> IoTypes]
    /\ shutdown \in BOOLEAN
    /\ free_slots \subseteq SlabIndices

(* ---- Initial state ---- *)

Init ==
    /\ conns = [i \in SlabIndices |-> "Free"]
    /\ pending_io = [i \in SlabIndices |-> "None"]
    /\ shutdown = FALSE
    /\ free_slots = SlabIndices

(* ---- Actions ---- *)

(* Accept a new connection into a free slab slot. *)
Accept(i) ==
    /\ ~shutdown
    /\ i \in free_slots
    /\ conns' = [conns EXCEPT ![i] = "Headers"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "Recv"]
    /\ free_slots' = free_slots \ {i}
    /\ UNCHANGED shutdown

(* Recv completes: headers parsed, no body needed. *)
RecvHeadersDone(i) ==
    /\ conns[i] = "Headers"
    /\ pending_io[i] = "Recv"
    /\ conns' = [conns EXCEPT ![i] = "Dispatching"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ UNCHANGED <<shutdown, free_slots>>

(* Recv completes: headers parsed, body needed. *)
RecvHeadersBody(i) ==
    /\ conns[i] = "Headers"
    /\ pending_io[i] = "Recv"
    /\ conns' = [conns EXCEPT ![i] = "Body"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "Recv"]
    /\ UNCHANGED <<shutdown, free_slots>>

(* Recv completes: need more header data. *)
RecvNeedMore(i) ==
    /\ conns[i] = "Headers"
    /\ pending_io[i] = "Recv"
    /\ pending_io' = [pending_io EXCEPT ![i] = "Recv"]
    /\ UNCHANGED <<conns, shutdown, free_slots>>

(* Recv completes: body complete. *)
RecvBodyDone(i) ==
    /\ conns[i] = "Body"
    /\ pending_io[i] = "Recv"
    /\ conns' = [conns EXCEPT ![i] = "Dispatching"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ UNCHANGED <<shutdown, free_slots>>

(* Recv completes: need more body data. *)
RecvBodyMore(i) ==
    /\ conns[i] = "Body"
    /\ pending_io[i] = "Recv"
    /\ pending_io' = [pending_io EXCEPT ![i] = "Recv"]
    /\ UNCHANGED <<conns, shutdown, free_slots>>

(* Dispatch completes: start writing response. *)
DispatchDone(i) ==
    /\ conns[i] = "Dispatching"
    /\ pending_io[i] = "None"
    /\ conns' = [conns EXCEPT ![i] = "Writing"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "Write"]
    /\ UNCHANGED <<shutdown, free_slots>>

(* Write completes: more data to write. *)
WriteMore(i) ==
    /\ conns[i] = "Writing"
    /\ pending_io[i] = "Write"
    /\ pending_io' = [pending_io EXCEPT ![i] = "Write"]
    /\ UNCHANGED <<conns, shutdown, free_slots>>

(* Write completes: response done, keep-alive -> back to Headers. *)
WriteKeepAlive(i) ==
    /\ conns[i] = "Writing"
    /\ pending_io[i] = "Write"
    /\ conns' = [conns EXCEPT ![i] = "Headers"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "Recv"]
    /\ UNCHANGED <<shutdown, free_slots>>

(* Write completes: response done, close connection. *)
WriteClose(i) ==
    /\ conns[i] = "Writing"
    /\ pending_io[i] = "Write"
    /\ conns' = [conns EXCEPT ![i] = "Free"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ free_slots' = free_slots \cup {i}
    /\ UNCHANGED shutdown

(* Parse error during headers: write error response then close. *)
ParseError(i) ==
    /\ conns[i] = "Headers"
    /\ pending_io[i] = "Recv"
    /\ conns' = [conns EXCEPT ![i] = "Writing"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "Write"]
    /\ UNCHANGED <<shutdown, free_slots>>

(* I/O error on recv or write: close immediately. *)
IoError(i) ==
    /\ conns[i] \in {"Headers", "Body", "Writing"}
    /\ pending_io[i] \in {"Recv", "Write"}
    /\ conns' = [conns EXCEPT ![i] = "Free"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ free_slots' = free_slots \cup {i}
    /\ UNCHANGED shutdown

(* Timeout sweep: close connection that has no pending I/O. *)
TimeoutNoPending(i) ==
    /\ conns[i] \in {"Headers", "Body"}
    /\ pending_io[i] = "None"
    /\ conns' = [conns EXCEPT ![i] = "Free"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ free_slots' = free_slots \cup {i}
    /\ UNCHANGED shutdown

(* Timeout sweep: close fd but keep slab entry for pending CQE. *)
TimeoutWithPending(i) ==
    /\ conns[i] \in {"Headers", "Body"}
    /\ pending_io[i] \in {"Recv", "Write"}
    /\ conns' = [conns EXCEPT ![i] = "Closed"]
    /\ UNCHANGED <<pending_io, shutdown, free_slots>>

(* CQE arrives for a Closed connection: release the slab slot. *)
ClosedCqeArrives(i) ==
    /\ conns[i] = "Closed"
    /\ pending_io[i] \in {"Recv", "Write"}
    /\ conns' = [conns EXCEPT ![i] = "Free"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ free_slots' = free_slots \cup {i}
    /\ UNCHANGED shutdown

(* SQ full: close connection. *)
SqFull(i) ==
    /\ conns[i] \in {"Headers", "Body", "Writing"}
    /\ pending_io[i] = "None"
    /\ conns' = [conns EXCEPT ![i] = "Free"]
    /\ pending_io' = [pending_io EXCEPT ![i] = "None"]
    /\ free_slots' = free_slots \cup {i}
    /\ UNCHANGED shutdown

(* Shutdown signal. *)
SignalShutdown ==
    /\ ~shutdown
    /\ shutdown' = TRUE
    /\ UNCHANGED <<conns, pending_io, free_slots>>

(* ---- Next state relation ---- *)

Next ==
    \/ SignalShutdown
    \/ \E i \in SlabIndices :
        \/ Accept(i)
        \/ RecvHeadersDone(i)
        \/ RecvHeadersBody(i)
        \/ RecvNeedMore(i)
        \/ RecvBodyDone(i)
        \/ RecvBodyMore(i)
        \/ DispatchDone(i)
        \/ WriteMore(i)
        \/ WriteKeepAlive(i)
        \/ WriteClose(i)
        \/ ParseError(i)
        \/ IoError(i)
        \/ TimeoutNoPending(i)
        \/ TimeoutWithPending(i)
        \/ ClosedCqeArrives(i)
        \/ SqFull(i)

(* ---- Safety properties ---- *)

(* At most one pending I/O operation per connection at any time. *)
MutualExclusion ==
    \A i \in SlabIndices :
        pending_io[i] \in {"None", "Recv", "Write"}

(* Free slots have no state and no pending I/O. *)
FreeSlotConsistency ==
    \A i \in SlabIndices :
        (conns[i] = "Free") <=> (i \in free_slots)

(* Free connections never have pending I/O. *)
FreeMeansNoPending ==
    \A i \in SlabIndices :
        (conns[i] = "Free") => (pending_io[i] = "None")

(* Closed connections always have pending I/O (waiting for CQE). *)
ClosedMeansPending ==
    \A i \in SlabIndices :
        (conns[i] = "Closed") => (pending_io[i] \in {"Recv", "Write"})

(* No connection in Dispatching state has pending I/O
   (dispatch runs synchronously). *)
DispatchingNoPending ==
    \A i \in SlabIndices :
        (conns[i] = "Dispatching") => (pending_io[i] = "None")

(* A slab slot is never reused while a CQE is in flight.
   This is ensured because Closed slots are NOT in free_slots. *)
NoStaleReuse ==
    \A i \in SlabIndices :
        (conns[i] = "Closed") => (i \notin free_slots)

(* Combined safety invariant. *)
Safety ==
    /\ TypeOK
    /\ FreeSlotConsistency
    /\ FreeMeansNoPending
    /\ ClosedMeansPending
    /\ DispatchingNoPending
    /\ NoStaleReuse

(* ---- Liveness properties ---- *)

(* Every connection eventually reaches Free (no stuck connections). *)
EventuallyFree ==
    \A i \in SlabIndices :
        (conns[i] # "Free") ~> (conns[i] = "Free")

(* After shutdown, eventually all connections are free. *)
ShutdownDrain ==
    shutdown ~> (\A i \in SlabIndices : conns[i] = "Free")

(* Fairness: all actions are weakly fair (progress guarantee). *)
Fairness ==
    /\ WF_vars(Next)

(* Temporal specification with liveness. *)
Spec == Init /\ [][Next]_vars /\ Fairness

====
