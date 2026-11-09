export const title = "启动项修改后退出 - 处理已接受的修改再结束进程";
const names = {"Open": "窗口可用，修改可排队", "Closed": "窗口关闭，已接受的修改仍保留", "Draining": "退出前处理剩余修改", "Exited": "修改处理完毕，进程退出"};
export const state = v => v.phase === "Exited" && Object.keys(v.finished).length < v.accepted ? "进程提前退出，修改丢失" : v.phase === "Open" ? (v.accepted === 0 ? "启动项列表" : v.current !== 0 ? "后台正在修改启动项" : Object.keys(v.queue).length ? "修改已接受，等待执行" : "本轮修改已处理") : names[v.phase];
const steps = {"AcceptToggle": "接受一次启用或禁用", "Refresh": "请求刷新，保留修改队列", "BeginWrite": "后台开始下一次修改", "FinishWrite": "完成修改或记录该次失败", "CloseWindow": "关闭窗口，停止向界面通知", "DrainOnExit": "开始退出前排空队列", "Exit": "全部已接受修改处理完才退出", "Done": "已退出"};
export const step = (a, from, to) => a === "Exit" && Object.keys(to.finished).length < to.accepted ? "未处理完修改就直接退出" : steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
