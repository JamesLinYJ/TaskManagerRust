---- MODULE ApplicationsLegacy ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, generation, applied
vars == <<phase, generation, applied>>

Init ==
    /\ phase = "Selected"
    /\ generation = 1
    /\ applied = 0

Confirm ==
    /\ phase = "Selected"
    /\ phase' = ("Queued")
    /\ UNCHANGED <<generation, applied>>

RecycleBeforeCheck ==
    /\ phase = "Queued" /\ generation = 1
    /\ generation' = (2)
    /\ UNCHANGED <<phase, applied>>

CheckIdentity ==
    /\ phase = "Queued"
    /\ phase' = (IF Broken \/ generation = 1 THEN "Checked" ELSE "Skipped")
    /\ UNCHANGED <<generation, applied>>

RecycleAfterCheck ==
    /\ phase = "Checked" /\ generation = 1
    /\ generation' = (2)
    /\ UNCHANGED <<phase, applied>>

CloseWindow ==
    /\ phase = "Checked"
    /\ phase' = ("Applied")
    /\ applied' = (generation)
    /\ UNCHANGED <<generation>>

Close ==
    /\ phase = "Queued"
    /\ phase' = ("Cancelled")
    /\ UNCHANGED <<generation, applied>>

Done ==
    /\ phase \in {"Applied", "Skipped", "Cancelled"}
    /\ UNCHANGED <<phase, generation, applied>>

Next == Confirm \/ RecycleBeforeCheck \/ CheckIdentity \/ RecycleAfterCheck \/ CloseWindow \/ Close \/ Done

Safety == applied # 2
====
