---- MODULE Startup ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, accepted, queue, current, finished
vars == <<phase, accepted, queue, current, finished>>

Init ==
    /\ phase = "Open"
    /\ accepted = 0
    /\ queue = <<>>
    /\ current = 0
    /\ finished = <<>>

AcceptToggle ==
    /\ phase = "Open" /\ accepted < 2
    /\ accepted' = (accepted + 1)
    /\ queue' = (Append(queue, accepted + 1))
    /\ UNCHANGED <<phase, current, finished>>

Refresh ==
    /\ phase = "Open"
    /\ UNCHANGED <<phase, accepted, queue, current, finished>>

BeginWrite ==
    /\ phase # "Exited" /\ current = 0 /\ Len(queue) > 0
    /\ current' = (Head(queue))
    /\ queue' = (Tail(queue))
    /\ UNCHANGED <<phase, accepted, finished>>

FinishWrite ==
    /\ current # 0 /\ phase # "Exited"
    /\ current' = (0)
    /\ finished' = (Append(finished, current))
    /\ UNCHANGED <<phase, accepted, queue>>

CloseWindow ==
    /\ phase = "Open"
    /\ phase' = ("Closed")
    /\ UNCHANGED <<accepted, queue, current, finished>>

DrainOnExit ==
    /\ phase = "Closed"
    /\ phase' = ("Draining")
    /\ UNCHANGED <<accepted, queue, current, finished>>

Exit ==
    /\ phase \in {"Closed", "Draining"} /\ (Broken \/ (phase = "Draining" /\ current = 0 /\ Len(queue) = 0))
    /\ phase' = ("Exited")
    /\ UNCHANGED <<accepted, queue, current, finished>>

Done ==
    /\ phase = "Exited"
    /\ UNCHANGED <<phase, accepted, queue, current, finished>>

Next == AcceptToggle \/ Refresh \/ BeginWrite \/ FinishWrite \/ CloseWindow \/ DrainOnExit \/ Exit \/ Done

Safety == (finished = SubSeq(<<1, 2>>, 1, Len(finished))) /\ (phase = "Exited" => finished = SubSeq(<<1, 2>>, 1, accepted))
====
