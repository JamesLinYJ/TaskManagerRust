---- MODULE Processes ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, generation, handle, applied
vars == <<phase, generation, handle, applied>>

Init ==
    /\ phase = "Selected"
    /\ generation = 1
    /\ handle = 0
    /\ applied = 0

Confirm ==
    /\ phase = "Selected"
    /\ phase' = ("Queued")
    /\ UNCHANGED <<generation, handle, applied>>

ReusePid ==
    /\ handle = 0 /\ generation = 1 /\ phase \in {"Selected", "Queued"}
    /\ generation' = (2)
    /\ UNCHANGED <<phase, handle, applied>>

OpenProcess ==
    /\ phase = "Queued"
    /\ handle' = (generation)
    /\ phase' = ("Opened")
    /\ UNCHANGED <<generation, applied>>

CheckCreationTime ==
    /\ phase = "Opened"
    /\ phase' = (IF Broken \/ handle = 1 THEN "Verified" ELSE "Failed")
    /\ UNCHANGED <<generation, handle, applied>>

Apply ==
    /\ phase = "Verified"
    /\ phase' = ("Applied")
    /\ applied' = (handle)
    /\ UNCHANGED <<generation, handle>>

ApiFailure ==
    /\ phase = "Verified"
    /\ phase' = ("Failed")
    /\ UNCHANGED <<generation, handle, applied>>

Close ==
    /\ phase \in {"Selected", "Queued"}
    /\ phase' = ("Cancelled")
    /\ UNCHANGED <<generation, handle, applied>>

Done ==
    /\ phase \in {"Applied", "Failed", "Cancelled"}
    /\ UNCHANGED <<phase, generation, handle, applied>>

Next == Confirm \/ ReusePid \/ OpenProcess \/ CheckCreationTime \/ Apply \/ ApiFailure \/ Close \/ Done

Safety == applied # 2
====
