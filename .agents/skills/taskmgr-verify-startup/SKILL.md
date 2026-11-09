---
name: taskmgr-verify-startup
description: "检查 TaskManagerRust 启动项修改后退出：处理已接受的修改再结束进程的有限状态性质；修改所列状态转换、队列或生命周期代码时使用。"
---

# 启动项修改后退出：处理已接受的修改再结束进程

保证：正常退出前，按顺序处理每一个已经接受的启动项修改。

破坏后的用户影响：否则窗口关闭会静默丢失用户已经提交的设置。

## 源码与模型对应

同步提交：`7003888753316de6662996112f8e3100ee470208`（2026-11-09）。逐文件 SHA-256 记录在 `verification/formal/models.json`。

| 源码 | 模型边界 |
| --- | --- |
| `src/infrastructure/worker.rs` | defer_drain_until_exit / into_exit_drain / drain_before_process_exit |
| `src/main.rs` | main 中 UI 返回到进程退出的顺序 |
| `src/pages/startup.rs` | destroy / submit |
| `src/pages/startup/model.rs` | merge_startup_requests |
| `src/pages/startup/sampler.rs` | perform 的顺序处理 |

模型：`Startup.tla`；正常配置：`Startup.cfg`；负例配置：`Startup-broken.cfg`；中文视图：`view.mjs`。

## 运行

先跟随本技能目录的符号链接定位实际检出目录（同时含 `Cargo.toml` 和 `verification/formal/verify.py`），再从该仓库根目录执行。不要误用外层工作区或其他旧检出目录：

```sh
python3 verification/formal/verify.py startup --self-test
```

需要 Python 3.9+、Rust 1.99+、Java 11+ 和校验过 SHA-256 的 `tla2tools` 1.7.4。工具查找顺序为 `--jar`、`TLA2TOOLS`、`verification/.tools/tla2tools-1.7.4.jar`。工具哈希、来源和首次准备方法见 `verification/formal/TOOLCHAIN.md`。输出自动放到新的系统临时目录，也可以用 `--out` 指定仓库外的新目录。

当前模型和真实源码重放应通过。自检中的 TLA+ 与 Rust 负例必须失败；这两种预期失败不是当前代码失败。

自检会在临时源码副本里从 main 删除 drain_before_process_exit 调用。它编译实际 Rust 函数体，验证断言确实失败，然后用未改动源码重新运行。它不修改产品代码。Win32 边界是受控替身；worker 重放用标准库单消费者有界通道替代 crossbeam，保留实际 worker 算法，不能外推为两种通道完全等价的证明。

对每次 TLA+ 结果使用 `/statechart`：通过时展示状态转换，失败时解释最短反例。读取当前安装的 `/statechart` 技能，不在这里固定其命令或安装路径；在仓库外复制模型、配置和 `view.mjs` 后运行它。

## 范围与维护

最多两次已接受修改，FIFO；修改失败也算已处理并留下失败结果。假设 collector 不 panic、线程可启动、系统 API 最终返回；不承诺每个写入成功或强制关机时持久化。

TLC 穷尽的是这个有限模型，不是整个 Rust 程序。API 原子性、调度公平性和环境动作只能按源码中实际边界建模；不要把“检查后再调用”合并成一步。未检查无界活性或真实设备行为。

遇到 `SOURCE DRIFT` 先比对本表函数与模型，判断是预期设计改变还是回归；不要仅刷新哈希让检查变绿。预期改变时同步状态、动作、性质和真实源码重放，保留负例敏感性，再更新同步提交和哈希。非预期失败保留反例，报告源码中的事件顺序，不弱化性质。产品修复需按当前用户任务授权处理。后续维护可使用 `/maintain-formal-verification-skill`。
