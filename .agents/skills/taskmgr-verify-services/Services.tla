---- MODULE Services ----
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT Broken
VARIABLES phase, stopped, started, ticks
vars == <<phase, stopped, started, ticks>>

Init ==
    /\ phase = "Selected"
    /\ stopped = FALSE
    /\ started = FALSE
    /\ ticks = 0

Confirm ==
    /\ phase = "Selected"
    /\ phase' = ("Stopping")
    /\ UNCHANGED <<stopped, started, ticks>>

RejectControl ==
    /\ phase = "Stopping"
    /\ phase' = ("Failed")
    /\ UNCHANGED <<stopped, started, ticks>>

AcceptStop ==
    /\ phase = "Stopping"
    /\ phase' = ("Waiting")
    /\ UNCHANGED <<stopped, started, ticks>>

ObserveOldRunning ==
    /\ phase = "Waiting" /\ ticks < 2
    /\ ticks' = (ticks + 1)
    /\ phase' = (IF Broken THEN "Success" ELSE "Waiting")
    /\ UNCHANGED <<stopped, started>>

Progress ==
    /\ phase = "Waiting" /\ ticks < 2
    /\ ticks' = (ticks + 1)
    /\ UNCHANGED <<phase, stopped, started>>

ObserveStopped ==
    /\ phase = "Waiting"
    /\ phase' = ("Stopped")
    /\ stopped' = (TRUE)
    /\ UNCHANGED <<started, ticks>>

Timeout ==
    /\ phase = "Waiting" /\ ticks = 2
    /\ phase' = ("Failed")
    /\ UNCHANGED <<stopped, started, ticks>>

QueryFailure ==
    /\ phase = "Waiting"
    /\ phase' = ("Failed")
    /\ UNCHANGED <<stopped, started, ticks>>

Cancel ==
    /\ phase \in {"Stopping", "Waiting", "Stopped"}
    /\ phase' = ("Cancelled")
    /\ UNCHANGED <<stopped, started, ticks>>

Start ==
    /\ phase = "Stopped"
    /\ phase' = ("Starting")
    /\ started' = (TRUE)
    /\ UNCHANGED <<stopped, ticks>>

StartAccepted ==
    /\ phase = "Starting"
    /\ phase' = ("Success")
    /\ UNCHANGED <<stopped, started, ticks>>

StartFailed ==
    /\ phase = "Starting"
    /\ phase' = ("Failed")
    /\ UNCHANGED <<stopped, started, ticks>>

Done ==
    /\ phase \in {"Success", "Failed", "Cancelled"}
    /\ UNCHANGED <<phase, stopped, started, ticks>>

Next == Confirm \/ RejectControl \/ AcceptStop \/ ObserveOldRunning \/ Progress \/ ObserveStopped \/ Timeout \/ QueryFailure \/ Cancel \/ Start \/ StartAccepted \/ StartFailed \/ Done

Safety == (started => stopped) /\ (phase = "Success" => (stopped /\ started))
====
