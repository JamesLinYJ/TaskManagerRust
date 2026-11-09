export const title = "服务重启 - 只有完成停止后才允许启动";
const names = {"Selected": "选中可重启的服务", "Stopping": "提交停止命令", "Waiting": "等待停止，跟踪进度", "Stopped": "已确认服务停止", "Starting": "提交启动命令", "Success": "启动已接受，等待刷新", "Failed": "操作失败或等待超时", "Cancelled": "关闭程序，中止后续启动"};
export const state = v => v.phase === "Success" && (!v.stopped || !v.started) ? "未启动却报告重启成功" : names[v.phase];
const steps = {"Confirm": "点击重启", "RejectControl": "停止命令失败", "AcceptStop": "停止命令已接受", "ObserveOldRunning": "查询仍返回旧的运行状态", "Progress": "服务报告停止进度", "ObserveStopped": "查询确认已停止", "Timeout": "停止等待达到上限", "QueryFailure": "状态查询失败", "Cancel": "关闭程序，取消等待", "Start": "发送启动命令", "StartAccepted": "查询为运行中或启动中", "StartFailed": "启动失败或状态不符", "Done": "保留操作结果"};
export const step = a => steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
