# Process architecture labels

The Processes and Applications pages share architecture detection and label formatting.
The original image name/window title remains the sort key. The architecture of taskmgr.exe
itself does not determine the architecture of the processes it observes.

| Host / main program | English suffix | Simplified Chinese suffix |
| --- | --- | --- |
| ARM64 / x86 | `(x86 emulated)` | `（x86 仿真）` |
| ARM64 / x86-64 | `(x86-64 emulated)` | `（x86-64 仿真）` |
| ARM64 / ARM64EC | `(ARM64EC)` | `（ARM64EC）` |
| ARM64 / native ARM64 | None | None |
| 64-bit host / supported 32-bit compatibility mode | Existing `(32-bit)` | Existing `(32位)` |
| Native 32-bit Windows / x86 | None | None |
| Query unavailable, access denied, unsupported or invalid image metadata | None; row error | None; row error |

No suffix is **not** proof of native execution. In particular, Windows 10 ARM64 can return
insufficient information to distinguish a native 64-bit process from an emulated one.
The label describes the main executable, not the instruction mix in every loaded DLL or JIT.
An ARM64EC main executable can contain or load both ARM64EC and x86-64 code.

## Sources and failure handling

- [IsWow64Process2](https://learn.microsoft.com/windows/win32/api/wow64apiset/nf-wow64apiset-iswow64process2)
  provides the native host machine and the WOW process machine. `UNKNOWN` means non-WOW64;
  it must not be interpreted as ARM64 native.
- [GetProcessInformation / ProcessMachineTypeInfo](https://learn.microsoft.com/windows/win32/api/processthreadsapi/ns-processthreadsapi-process_machine_information)
  provides the process machine on Windows 11. If the information class is unsupported, only
  facts established by the older API are retained. Other API failures remain errors.
- For AMD64 processes on ARM64, read the main executable's PE load configuration and CHPE
  metadata pointer to distinguish [ARM64EC](https://learn.microsoft.com/windows/arm/arm64ec).
  The final EXE uses an AMD64 machine field; `0xA641` identifies an intermediate MSVC object,
  not a final executable. PE offsets, sizes, RVAs and file boundaries are validated before use.
  Managed AnyCPU images retain their queried runtime architecture.
- A 32-bit observer temporarily disables WOW64 file-system redirection only while opening the
  queried main image, then immediately restores it. Otherwise System32 could resolve to a
  different SysWOW64 executable. Images are read as files, never loaded as executable modules.
- All queries run on existing background workers. Successful results are cached by
  `ProcIdentity` (PID and creation time), and removed when that identity exits. Failures do not
  become successful/native cache entries or discard other rows.

## Reproduce the ARM64 acceptance test

Compile `tests/fixtures/architecture_window.cpp` in MSVC developer environments for ARM64,
x86 and x86-64, producing `fixture-arm64.exe`, `fixture-x86.exe` and `fixture-x64.exe` in a
single fixture directory. Use `/W4 /WX /O2 /MT /utf-8 /DUNICODE /D_UNICODE` and link with
`user32.lib`, `kernel32.lib`, `/SUBSYSTEM:WINDOWS` and the matching `/MACHINE` option.
For `fixture-arm64ec.exe`, use the ARM64 developer environment, compile with `/arm64EC /c`,
then link separately with `/MACHINE:ARM64EC`; the library search path must include the MSVC
ARM64EC support directory and ARM64 CRT/SDK libraries. These are normal compiled window
programs, with no production test hooks. Set `TASKMGR_ARCH_FIXTURES` to their directory.

Run this test for each observer target: `aarch64-pc-windows-msvc`,
`x86_64-pc-windows-msvc`, and `i686-pc-windows-msvc`:

```powershell
cargo test --locked --bin taskmgr --target aarch64-pc-windows-msvc `
  live_architecture_fixtures_use_the_real_process_sampler -- --ignored --test-threads=1
```

The test owns and cleans up its child processes, waits for their actual GUI readiness,
checks both direct identity-verified queries and two real sampler passes, rejects a mismatched
creation time, then verifies the exited fixtures no longer appear. Ordinary unit tests also
cover machine classification, unknown API results, malformed/truncated PE data, AnyCPU
metadata, suffix formatting, and incremental display invalidation.

For UI acceptance, start these fixtures in the interactive desktop and run each release
observer separately. Confirm both pages show the same four classifications, then exercise
sorting, refreshing, page switching, minimize/restore and clean exit. Record the exact Windows
build, observer targets, source/binary hashes and screenshots in the compatibility evidence.
Do not equate this targeted check with the complete hardware compatibility checklist.
