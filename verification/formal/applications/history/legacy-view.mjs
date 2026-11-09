export const title = "结束应用 - 检查窗口之后的复用间隙";
const names = {"Selected": "选中应用窗口", "Queued": "确认结束任务，后台排队", "Checked": "窗口身份检查通过", "Skipped": "目标身份已变化，跳过", "Applied": "已发送关闭请求", "Cancelled": "操作尚未开始时关闭程序"};
export const state = v => v.phase === "Applied" && v.applied === 2 ? "误操作了复用后的窗口" : (v.phase === "Checked" && v.generation === 2 ? "已进入调用阶段，窗口身份已变化" : names[v.phase]);
const steps = {"Confirm": "点击结束任务", "RecycleBeforeCheck": "窗口在检查前被复用", "CheckIdentity": "核对窗口、线程与进程身份", "RecycleAfterCheck": "检查返回后，窗口销毁并被复用", "CloseWindow": "按原 HWND 发送关闭消息", "Close": "关闭程序，取消尚未开始的请求", "Done": "处理结束"};
export const step = (a, from, to) => a === "CheckIdentity" && from.generation === 2 && to.phase === "Checked" ? "身份不匹配仍放行" : steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
