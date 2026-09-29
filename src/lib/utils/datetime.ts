/**
 * 时间显示格式化。
 *
 * 目标是把 RFC3339 时间戳渲染成中文直觉表达，而不是 `2026-09-29T14:30:00+08:00`。
 *
 * 关于"日期边界"的实现取舍：判断"今天/明天"时，我把两个时间都归一到当天的
 * 零点再相减，而不是比较 `getDate()` 的差值。
 * 后者在跨月（9月30日 → 10月1日）和跨年时会得出错误的差值；
 * 用零点时间戳相减则天然正确，且不依赖任何手写的月份天数表。
 */

/** 一天的毫秒数。仅用于日期边界运算，不用于时间加减。 */
const DAY_MS = 86_400_000;

/** 解析 RFC3339 时间戳。无法解析时返回 null，由调用方决定如何降级。 */
export function parseTime(iso: string | null): Date | null {
  if (!iso) return null;
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? null : d;
}

/** 取某天零点的时间戳。用于计算日期差，规避跨月/跨年的边界错误。 */
function startOfDay(d: Date): number {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/** 日期差（以天为单位，按本地日历天计算）。 */
export function dayDiff(target: Date, base: Date = new Date()): number {
  return Math.round((startOfDay(target) - startOfDay(base)) / DAY_MS);
}

/** 补零。 */
function pad(n: number): string {
  return n.toString().padStart(2, '0');
}

/** `HH:MM`。 */
export function formatClock(d: Date): string {
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * 主要的时间标签，例如「今天 14:30」「明天 09:00」「周三 14:30」。
 *
 * 一周以外的日期直接给出月日，因为"下下周三"这种说法本身就不如日期直观。
 */
export function formatTimeLabel(iso: string | null): string {
  const d = parseTime(iso);
  if (!d) return '';

  const diff = dayDiff(d);

  if (diff === 0) return `今天 ${formatClock(d)}`;
  if (diff === 1) return `明天 ${formatClock(d)}`;
  if (diff === 2) return `后天 ${formatClock(d)}`;
  if (diff === -1) return `昨天 ${formatClock(d)}`;

  // 未来一周内用星期几，更符合"这周还要干什么"的思维
  if (diff > 2 && diff < 7) {
    const weekday = new Intl.DateTimeFormat('zh-CN', { weekday: 'short' }).format(d);
    return `${weekday} ${formatClock(d)}`;
  }

  const md = new Intl.DateTimeFormat('zh-CN', { month: 'long', day: 'numeric' }).format(d);
  // 跨年时补上年份，否则"1月5日"会有歧义
  const sameYear = d.getFullYear() === new Date().getFullYear();
  const prefix = sameYear ? md : `${d.getFullYear()}年${md}`;
  return `${prefix} ${formatClock(d)}`;
}

/** 仅日期，例如「今天」「明天」「9月30日」。全天任务使用。 */
export function formatDateLabel(iso: string | null): string {
  const d = parseTime(iso);
  if (!d) return '';

  const diff = dayDiff(d);
  if (diff === 0) return '今天';
  if (diff === 1) return '明天';
  if (diff === 2) return '后天';
  if (diff === -1) return '昨天';

  const md = new Intl.DateTimeFormat('zh-CN', { month: 'long', day: 'numeric' }).format(d);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  return sameYear ? md : `${d.getFullYear()}年${md}`;
}

/**
 * 剩余时间的倒计时文本，例如「还剩 2 天 5 小时」「还剩 23 分钟」「已逾期 3 小时」。
 *
 * 精度随剩余时间长短自动降级：还有几天时精确到小时没有意义，
 * 而只剩几分钟时必须精确到分钟，否则用户无法判断是否还来得及。
 */
export function formatCountdown(iso: string | null, now: Date = new Date()): string {
  const d = parseTime(iso);
  if (!d) return '';

  const ms = d.getTime() - now.getTime();
  const overdue = ms < 0;
  const abs = Math.abs(ms);

  const minutes = Math.floor(abs / 60_000);
  const hours = Math.floor(abs / 3_600_000);
  const days = Math.floor(abs / DAY_MS);

  let body: string;
  if (days >= 1) {
    const restHours = hours - days * 24;
    body = restHours > 0 ? `${days} 天 ${restHours} 小时` : `${days} 天`;
  } else if (hours >= 1) {
    const restMinutes = minutes - hours * 60;
    body = restMinutes > 0 ? `${hours} 小时 ${restMinutes} 分钟` : `${hours} 小时`;
  } else if (minutes >= 1) {
    body = `${minutes} 分钟`;
  } else {
    body = '不到 1 分钟';
  }

  return overdue ? `已逾期 ${body}` : `还剩 ${body}`;
}

/** 紧迫度分档。决定倒计时用什么颜色，对应 tokens.css 的 --color-urgency-*。 */
export type Urgency = 'relaxed' | 'soon' | 'overdue';

/**
 * 按剩余时间分档。
 *
 * 阈值的选择依据是"用户还需要改变行为的程度"：
 *   超过 1 天 → 还有调整空间，中性色
 *   1 天以内 → 需要开始动手了，警示橙
 *   已过期   → 危险红
 */
export function urgencyOf(iso: string | null, now: Date = new Date()): Urgency {
  const d = parseTime(iso);
  if (!d) return 'relaxed';

  const ms = d.getTime() - now.getTime();
  if (ms < 0) return 'overdue';
  if (ms <= DAY_MS) return 'soon';
  return 'relaxed';
}
