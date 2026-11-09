export const title = "性能采样 - 缺失或无效数据不得画成真实零值";
const names = {"Ready": "保留上一轮可信曲线", "Sampling": "后台读取性能数据", "Candidate": "检查数据有效性", "Fresh": "追加真实采样点，包括真实零值", "Stale": "保持旧曲线，显示未知或未刷新", "BaselineOnly": "零间隔磁盘采样仅推进基线"};
export const state = v => v.history > 1 && !v.eligible ? "未知容量被画成真实零值" : names[v.phase];
const steps = {"ChooseCpu": "采样 CPU", "ChooseMemory": "采样物理内存", "ChooseGpu": "采样 GPU 显存", "ChooseDisk": "采样磁盘", "ValidSample": "读数及计算前提有效", "MissingOrInvalid": "数据缺失、容量未知或间隔无效", "Publish": "追加本次历史点", "ZeroDiskInterval": "磁盘读取成功但间隔为零，仅推进基线", "RetainHistory": "不追加伪造的零值", "Done": "等待下一次采样"};
export const step = a => steps[a] ?? a;
export const group = v => ["Failed", "Cancelled", "Skipped", "Stale"].includes(v.phase) ? "失败或取消" : ["Success", "Exited", "Done", "Applied", "Fresh"].includes(v.phase) ? "处理完成" : "处理过程";
export const colors = {"处理过程":"blue", "处理完成":"teal", "失败或取消":"orange"};
