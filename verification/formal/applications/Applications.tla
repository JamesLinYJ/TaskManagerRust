---- MODULE Applications ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, generation, handle, applied, confirmed, windowGeneration
vars == <<phase, generation, handle, applied, confirmed, windowGeneration>>

Init ==
    /\ phase = "Selected"
    /\ generation = 1
    /\ handle = 0
    /\ applied = 0
    /\ confirmed = FALSE
    /\ windowGeneration = 1

ShowWarning ==
    /\ phase = "Selected"
    /\ phase' = "Confirming"
    /\ UNCHANGED <<generation, handle, applied, confirmed, windowGeneration>>

Confirm ==
    /\ phase = "Confirming"
    /\ confirmed' = TRUE
    /\ phase' = ("Queued")
    /\ UNCHANGED <<generation, handle, applied, windowGeneration>>

ReusePid ==
    /\ handle = 0 /\ generation = 1 /\ phase \in {"Selected", "Confirming", "Queued"}
    /\ generation' = (2)
    /\ UNCHANGED <<phase, handle, applied, confirmed, windowGeneration>>

OpenProcess ==
    /\ phase = "Queued"
    /\ handle' = (generation)
    /\ phase' = ("Opened")
    /\ UNCHANGED <<generation, applied, confirmed, windowGeneration>>

CheckCreationTime ==
    /\ phase = "Opened"
    /\ phase' = (IF Broken \/ handle = 1 THEN "Verified" ELSE "Failed")
    /\ UNCHANGED <<generation, handle, applied, confirmed, windowGeneration>>

Apply ==
    /\ phase = "Verified"
    /\ phase' = ("Applied")
    /\ applied' = (handle)
    /\ UNCHANGED <<generation, handle, confirmed, windowGeneration>>

ApiFailure ==
    /\ phase = "Verified"
    /\ phase' = ("Failed")
    /\ UNCHANGED <<generation, handle, applied, confirmed, windowGeneration>>

Close ==
    /\ phase \in {"Selected", "Confirming", "Queued"}
    /\ phase' = ("Cancelled")
    /\ UNCHANGED <<generation, handle, applied, confirmed, windowGeneration>>

Done ==
    /\ phase \in {"Applied", "Failed", "Cancelled"}
    /\ UNCHANGED <<phase, generation, handle, applied, confirmed, windowGeneration>>

RecycleWindow ==
    /\ windowGeneration = 1
    /\ phase \in {"Confirming", "Queued", "Opened", "Verified"}
    /\ windowGeneration' = 2
    /\ UNCHANGED <<phase, generation, handle, applied, confirmed>>

CancelConfirmation ==
    /\ phase = "Confirming"
    /\ phase' = "Cancelled"
    /\ UNCHANGED <<generation, handle, applied, confirmed, windowGeneration>>

OpenFailure ==
    /\ phase = "Queued"
    /\ phase' = "Failed"
    /\ UNCHANGED <<generation, handle, applied, confirmed, windowGeneration>>

Next == ShowWarning \/ RecycleWindow \/ CancelConfirmation \/ OpenFailure \/ Confirm \/ ReusePid \/ OpenProcess \/ CheckCreationTime \/ Apply \/ ApiFailure \/ Close \/ Done

Safety == applied = 0 \/ (confirmed /\ applied = 1)
====
