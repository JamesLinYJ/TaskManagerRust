---- MODULE Sampling ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, kind, eligible, history
vars == <<phase, kind, eligible, history>>

Init ==
    /\ phase = "Ready"
    /\ kind = "none"
    /\ eligible = FALSE
    /\ history = 1

ChooseCpu ==
    /\ phase = "Ready"
    /\ phase' = ("Sampling")
    /\ kind' = ("cpu")
    /\ UNCHANGED <<eligible, history>>

ChooseMemory ==
    /\ phase = "Ready"
    /\ phase' = ("Sampling")
    /\ kind' = ("memory")
    /\ UNCHANGED <<eligible, history>>

ChooseGpu ==
    /\ phase = "Ready"
    /\ phase' = ("Sampling")
    /\ kind' = ("gpu")
    /\ UNCHANGED <<eligible, history>>

ChooseDisk ==
    /\ phase = "Ready"
    /\ phase' = ("Sampling")
    /\ kind' = ("disk")
    /\ UNCHANGED <<eligible, history>>

ValidSample ==
    /\ phase = "Sampling"
    /\ phase' = ("Candidate")
    /\ eligible' = (TRUE)
    /\ UNCHANGED <<kind, history>>

MissingOrInvalid ==
    /\ phase = "Sampling"
    /\ phase' = ("Candidate")
    /\ eligible' = (FALSE)
    /\ UNCHANGED <<kind, history>>

Publish ==
    /\ phase = "Candidate" /\ (eligible \/ (Broken /\ kind = "gpu"))
    /\ phase' = ("Fresh")
    /\ history' = (history + 1)
    /\ UNCHANGED <<kind, eligible>>

ZeroDiskInterval ==
    /\ phase = "Sampling" /\ kind = "disk"
    /\ phase' = ("BaselineOnly")
    /\ UNCHANGED <<kind, eligible, history>>

RetainHistory ==
    /\ phase = "Candidate" /\ ~eligible /\ ~(Broken /\ kind = "gpu")
    /\ phase' = ("Stale")
    /\ UNCHANGED <<kind, eligible, history>>

Done ==
    /\ phase \in {"Fresh", "Stale", "BaselineOnly"}
    /\ UNCHANGED <<phase, kind, eligible, history>>

Next == ChooseCpu \/ ChooseMemory \/ ChooseGpu \/ ChooseDisk \/ ValidSample \/ MissingOrInvalid \/ Publish \/ ZeroDiskInterval \/ RetainHistory \/ Done

Safety == history > 1 => eligible
====
