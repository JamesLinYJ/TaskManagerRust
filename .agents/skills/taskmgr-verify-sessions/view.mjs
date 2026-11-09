export const title = "用户会话 - 消息逐次保留, 注销和断开去重";
const names = {"Ready": "选择用户会话", "Queued": "消息已接受，等待逐次处理", "Done": "已逐条完成或报告失败"};
export const state = v => Object.keys(v.queue).length + Object.keys(v.reported).length < v.submitted ? "已确认消息被静默丢弃" : names[v.phase];
const steps = {"SendMessage": "确认发送一条消息", "QueueDisconnect": "接受注销或断开命令", "RepeatDisconnect": "重复注销或断开被合并", "ReportMessage": "核对会话并执行，或报告失败", "Done": "等待后续用户操作"};
export const step = a => steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
