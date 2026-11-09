---- MODULE Worker ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, gate, work, late
vars == <<phase, gate, work, late>>

Init ==
    /\ phase = "Open"
    /\ gate = TRUE
    /\ work = "idle"
    /\ late = FALSE

Submit ==
    /\ phase = "Open" /\ work = "idle"
    /\ work' = ("running")
    /\ UNCHANGED <<phase, gate, late>>

Complete ==
    /\ work = "running"
    /\ work' = ("ready")
    /\ UNCHANGED <<phase, gate, late>>

Notify ==
    /\ work = "ready"
    /\ work' = ("notified")
    /\ late' = (late \/ (gate /\ phase = "Gone"))
    /\ UNCHANGED <<phase, gate>>

Retire ==
    /\ phase = "Open"
    /\ phase' = ("Retired")
    /\ gate' = (IF Broken THEN gate ELSE FALSE)
    /\ UNCHANGED <<work, late>>

DestroyWindow ==
    /\ phase = "Retired"
    /\ phase' = ("Gone")
    /\ UNCHANGED <<gate, work, late>>

Done ==
    /\ phase = "Gone" /\ work \in {"idle", "notified"}
    /\ UNCHANGED <<phase, gate, work, late>>

Next == Submit \/ Complete \/ Notify \/ Retire \/ DestroyWindow \/ Done

Safety == ~late
====
