# 应用程序结束任务验证

这是普通仓库验证资料，不是 agent 技能。

执行 `python3 verification/formal/verify.py applications --self-test`，预期退出 0。
源码范围与逐文件 SHA-256 见上层 models.json；基线 7003888753316de6662996112f8e3100ee470208 加当前工作区修改。

## 性质与映射

用户确认后，仅终止选择时保存身份对应的进程对象。applications.rs 中 request_end_task 统一按钮、Delete 和上下文菜单；confirmed_end_task_request 先复制 ProcIdentity 再确认，取消不提交。EndTaskRequest 只保留进程身份；run_end_tasks、run_end_task_queue 和 end_task_processes 顺序执行并按每次请求完整身份去重。terminate_task_process 经 process_identity.rs 的 open_process_for_identity 核对创建时间后，使用同一 owned handle 终止。没有窗口关闭消息或 Ctrl 特殊分支。

模型 Applications.tla 描述一个保存身份、一个 PID 的两个创建时间、一个窗口的两个代际、一次确认和一个拥有句柄。窗口复用动作保留，但不参与终止目标选择。Windows 句柄固定进程对象是平台前提。队列多目标、具体 API 错误码和确认框视觉交互不由单目标模型全部证明。

实际源码重放包含确认、排队和终止函数；Win32 替身模拟未知身份、PID 复用、窗口更换目标、API 失败和验证后外部查找变化。自检在临时副本移除创建时间守卫，必须检测到断言失败，再恢复重跑。Windows 原生测试只终止自建测试子进程。程序退出可取消未开始请求，进行中操作完成后的通知由 worker gate 封闭。

## 变更与历史

旧承诺是“发送普通关闭消息时仍是原窗口”，确有检查与使用间隙。用户要求彻底修复后，产品改为每次确认后结束整个进程，允许失去应用保存提示。新承诺是“确认后只终止保存身份对应的进程”。旧路径已删除，旧模型位于 history/，不能计为当前代码结果。当前状态图输入为 Applications.tla、Applications.cfg、view.mjs；负例使用 Applications-broken.cfg。

源码变化后先核对模型和重放，再更新哈希；不能通过删除环境动作或仅刷新哈希制造通过。输出放到仓库外；工具版本见 ../TOOLCHAIN.md。这是有限模型及受控重放，不是整个程序或所有 Windows 版本的证明。
