# Formal verification toolchain

- TLC: `tla2tools` release **1.7.4** (TLC reports **2.19**, revision `5a47802`).
- Download: https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
- SHA-256: `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.
- Put the downloaded file at `verification/.tools/tla2tools-1.7.4.jar` (ignored by Git), or pass `--jar`, or set `TLA2TOOLS`.
- Java 11+; this run uses OpenJDK **25.0.4.1**. `JAVA` or `--java` selects the executable.
- Python 3.9+; this run uses **3.9.6**. Rust **1.99.0** compiles the actual-source replays without Cargo or network dependencies.
- `/statechart` produces the Mermaid statecharts. Read that installed skill to rerun; its invocation is deliberately not embedded in the project.
- Rendered SVGs use **Pele 1.9.0**, installed outside the repository. No product dependency changed.

Run every subsystem with `python3 verification/formal/verify.py all --self-test`.
Exit **0** means all selected current properties hold; **1** means a current invariant violation was found; **2** means source drift, a broken harness, missing tools, or an incomplete check. In the recorded baseline, Applications is a current finding, so the full suite exits 1. It is never relabeled as a passing result.

TLC may need permission to open a temporary local Java RMI socket even with one worker. A sandbox permission error is a tooling failure, not an invariant violation. Each run uses its own temporary directory to avoid shared Java module extraction races.
