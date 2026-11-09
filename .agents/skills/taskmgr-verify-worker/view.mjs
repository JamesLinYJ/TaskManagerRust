export const title = "后台任务 - 关闭通知, 再销毁窗口";
const names = {"Open": "页面可用", "Retired": "通知已关闭", "Gone": "页面已销毁"};
export const state = v => v.late ? "窗口销毁后仍收到通知" : ((v.phase !== "Open" && v.gate ? (v.phase === "Gone" ? "页面已销毁但通知入口仍开放" : "页面已退休但通知入口未关闭") : names[v.phase]) + " - " + ({idle:"无任务", running:"任务执行中", ready:"结果待通知", notified:"通知已处理"}[v.work]));
const steps = {"Submit": "提交后台任务", "Complete": "任务完成", "Notify": "尝试发送完成通知", "Retire": "关闭页面，封闭通知入口", "DestroyWindow": "销毁窗口", "Done": "退出完成"};
export const step = a => steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
