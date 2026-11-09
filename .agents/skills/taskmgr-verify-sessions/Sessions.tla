---- MODULE Sessions ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, submitted, queue, reported, disconnect
vars == <<phase, submitted, queue, reported, disconnect>>

Init ==
    /\ phase = "Ready"
    /\ submitted = 0
    /\ queue = <<>>
    /\ reported = <<>>
    /\ disconnect = FALSE

SendMessage ==
    /\ submitted < 2
    /\ submitted' = (submitted + 1)
    /\ queue' = (IF Broken /\ Len(queue) > 0 THEN queue ELSE Append(queue, submitted + 1))
    /\ phase' = ("Queued")
    /\ UNCHANGED <<reported, disconnect>>

QueueDisconnect ==
    /\ ~disconnect
    /\ disconnect' = (TRUE)
    /\ UNCHANGED <<phase, submitted, queue, reported>>

RepeatDisconnect ==
    /\ disconnect
    /\ UNCHANGED <<phase, submitted, queue, reported, disconnect>>

ReportMessage ==
    /\ Len(queue) > 0
    /\ reported' = (Append(reported, Head(queue)))
    /\ queue' = (Tail(queue))
    /\ phase' = (IF Len(queue) = 1 THEN "Done" ELSE "Queued")
    /\ UNCHANGED <<submitted, disconnect>>

Done ==
    /\ submitted = 2 /\ Len(queue) = 0
    /\ UNCHANGED <<phase, submitted, queue, reported, disconnect>>

Next == SendMessage \/ QueueDisconnect \/ RepeatDisconnect \/ ReportMessage \/ Done

Safety == reported \o queue = SubSeq(<<1, 2>>, 1, submitted)
====
