export const title = "结束应用进程 - 按创建时间核对, 再使用同一已打开句柄";
const names = {"Selected": "选中应用并保存进程身份", "Confirming": "等待确认结束整个进程", "Queued": "确认操作，后台排队", "Opened": "打开目标的进程句柄", "Verified": "创建时间匹配，持有该句柄", "Applied": "操作已执行", "Failed": "目标变化或操作失败", "Cancelled": "操作已取消，目标未结束"};
export const state = v => v.applied === 2 ? "误操作了复用后的进程" : v.phase === "Verified" && v.handle === 2 ? "身份检查被绕过，错误句柄被放行" : names[v.phase];
const steps = {"ShowWarning": "提示整个进程结束及数据丢失", "CancelConfirmation": "取消确认", "OpenFailure": "打开进程失败", "RecycleWindow": "窗口已变化，保存的进程身份不变", "Confirm": "确认结束应用进程", "ReusePid": "原进程退出，PID 被复用", "OpenProcess": "后台按 PID 打开进程", "CheckCreationTime": "比较保存与实际创建时间", "Apply": "使用核对后的同一进程句柄", "ApiFailure": "进程退出或系统调用失败", "Close": "关闭程序，取消尚未开始的请求", "Done": "保留执行结果"};
export const step = a => steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
