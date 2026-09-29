//! Windows 原生 Toast 通知。
//!
//! # 为什么直接调用 WinRT 而不使用 `tauri-plugin-notification`
//!
//! 该插件在 Windows 上不支持通知内的操作按钮，而本项目明确要求"通知里能直接
//! 完成 / 稍后提醒"。只有 WinRT 的 `ToastNotification` 能承载带按钮的 toast。
//!
//! # 关键限制：未打包应用需要先注册 AppUserModelID
//!
//! Windows 只会为"AUMID 已注册"的应用显示 toast。注册需要满足以下之一：
//!
//! 1. 存在一个带该 AUMID 的**开始菜单快捷方式**（安装包会创建）；
//! 2. 存在一个已注册的 **COM 通知激活器**（通常需要一个原生 DLL）；
//! 3. 由另一个"AUMID 已注册"的宿主进程代为弹出。
//!
//! 方案 1 在 `cargo tauri dev` 或直接运行 exe 时不成立 —— 这正是
//! "提醒到点了却看不到任何通知"的根因。方案 2 需要额外编译并随包分发一个 DLL，
//! 对一个待办应用来说过重。
//!
//! **因此这里采用方案 3**：通过 PowerShell 调用 WinRT 弹 toast。
//! Windows 10/11 自带的 PowerShell 本身就是已注册的宿主，
//! 由它弹通知不需要我们注册任何东西。
//!
//! # 设计取舍：尽力而为，失败只记日志
//!
//! 通知弹不出来（系统限制、通知被系统关闭、PowerShell 不可用）不应该影响
//! 应用的其他部分，更不该让进程退出。因此本模块所有失败路径都只写日志，
//! 不向上传播错误 —— 提醒是否被记录为"已触发"由调度器负责，
//! 与通知是否弹出无关。

use chrono::{DateTime, FixedOffset, Local};

/// 弹出提醒通知。失败时只打印日志。
pub fn show(task_title: &str, scheduled_at: DateTime<FixedOffset>) {
    if let Err(e) = try_show(task_title, scheduled_at) {
        eprintln!("弹出通知失败（不影响任务数据）：{e}");
    }
}

#[cfg(windows)]
fn try_show(task_title: &str, scheduled_at: DateTime<FixedOffset>) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    // 用 CREATE_NO_WINDOW 避免弹出黑色控制台窗口。
    // 用户收到提醒时不该先看到一个命令行窗口闪一下 —— 那会显得很不专业。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let time_label = format_time_label(scheduled_at);

    // 这里是 **XML 转义**，不是 PowerShell 转义。
    //
    // 内容被嵌进一个 here-string（`@'...'@`），那个形式下 PowerShell 不做任何
    // 展开，因此不需要也不应该加引号 —— 加了引号反而会让引号成为 XML 内容的一部分，
    // 通知里就会显示出多余的 `'`。真正需要处理的是 XML 元字符：
    // 任务标题里的 `&` 或 `<` 未经转义会让 `LoadXml` 直接解析失败，
    // 表现为"某些任务的通知弹不出来"，且只有含特殊字符的标题才复现。
    let title_xml = xml_escape("Todox 提醒");
    let body_xml = xml_escape(&format!("{task_title}\n{time_label}"));

    // 两个关键设计：
    //
    // 1. **`scenario="reminder"`** —— 这是 Windows 专门为"闹钟/提醒"提供的通知场景。
    //    普通 toast 几秒后自动消失，用户没看到就永远错过了；reminder 场景会让通知
    //    **持续停留直到用户主动处理**，并且在专注助手（勿扰）开启时仍能显示。
    //    这正是"提示要显示在最上层"所对应的系统能力。
    //
    // 2. **操作按钮** ——「打开 Todox」激活应用主窗口；「稍后提醒」把提醒推迟。
    //    两者都用自定义协议触发（见 lib.rs 的深链接处理）。
    //
    // 用自定义 XML 而不是内置模板：内置的 ToastText02 既不支持 scenario 也不支持按钮。
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null
$xml = @'
<toast scenario="reminder" activationType="protocol" launch="todox://open">
  <visual>
    <binding template="ToastGeneric">
      <text>{title_xml}</text>
      <text>{body_xml}</text>
    </binding>
  </visual>
  <actions>
    <action content="打开 Todox" activationType="protocol" arguments="todox://open" />
    <action content="稍后提醒" activationType="protocol" arguments="todox://snooze" />
  </actions>
</toast>
'@
$doc = [Windows.Data.Xml.Dom.XmlDocument]::new()
$doc.LoadXml($xml)
$toast = [Windows.UI.Notifications.ToastNotification]::new($doc)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Microsoft.Windows.PowerShell').Show($toast)
"#
    );

    // 用带超时的执行，而不是 `Command::output()`。
    //
    // `output()` 会无限期等待子进程退出。若 PowerShell 因杀软拦截、系统卡顿
    // 或自身挂起而永不返回，调用它的调度器线程就永久停摆 —— 之后所有提醒
    // 都不再触发，而用户只看到"提醒忽然不响了"。
    //
    // 本函数运行在调度器任务里，因此绝不能无限阻塞。
    let mut child = Command::new(powershell_path())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("无法启动 PowerShell：{e}"))?;

    // 轮询等待，最多 NOTIFY_TIMEOUT。
    // 用 50ms 的轮询间隔：这个循环只在"有提醒要弹"时运行，
    // 每次最多几十毫秒，不会影响空闲时的 CPU 指标（空闲时根本不会进这里）。
    let deadline = std::time::Instant::now() + NOTIFY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        use std::io::Read;
                        let _ = e.read_to_string(&mut err);
                    }
                    return Err(format!(
                        "PowerShell 退出码 {:?}：{}",
                        status.code(),
                        err.trim()
                    ));
                }
                return Ok(());
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // 超时：杀掉子进程，回收它，然后放弃这次通知。
                    //
                    // 这里**不返回错误到上层**（上层只记日志），因为通知失败
                    // 不应该影响这条提醒已被标记为"已触发"这个事实 ——
                    // 否则它会进入补发队列，变成反复尝试。
                    let _ = child.kill();
                    let _ = child.wait(); // 必须 wait，否则会留下僵尸进程
                    return Err(format!(
                        "PowerShell 在 {} 秒内未返回，已终止",
                        NOTIFY_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("等待 PowerShell 失败：{e}"));
            }
        }
    }
}

/// 通知子进程的超时时间。
///
/// 10 秒足够覆盖 PowerShell 冷启动（实测数百毫秒到数秒），
/// 又不至于让调度器停摆太久 —— 超时后这次通知被放弃，
/// 但提醒本身已标记为已触发，不会卡住后续提醒。
const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// PowerShell 可执行文件的绝对路径。
///
/// 用绝对路径而不是 `"powershell"`：相对名会让 Windows 依次搜索
/// exe 所在目录与当前工作目录，理论上可被同名文件劫持。
/// 虽然攻击者需要先能写这两个目录（那时能做更坏的事），
/// 但用绝对路径是零成本的加固。
fn powershell_path() -> std::path::PathBuf {
    // 优先用系统目录下的 Windows PowerShell；
    // 若不存在（极少见）则退回相对名，交由系统 PATH 解析。
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let candidate = std::path::Path::new(&system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");

    if candidate.exists() {
        candidate
    } else {
        std::path::PathBuf::from("powershell")
    }
}

/// 非 Windows 平台的空实现。
///
/// 保留它而不是用 `#[cfg]` 把调用点也包起来：调用方（调度器）的代码应当
/// 与平台无关，平台差异只在本模块内消化。
#[cfg(not(windows))]
fn try_show(_task_title: &str, _scheduled_at: DateTime<FixedOffset>) -> Result<(), String> {
    Ok(())
}

/// 把字符串转成可安全嵌入 XML 的形式。
///
/// 做两件事，缺一不可：
///
/// 1. **转义 XML 元字符**。未转义的 `&` 或 `<` 会让 `LoadXml` 解析失败。
///
/// 2. **剔除 XML 1.0 不允许的控制字符**。这一条同样致命，而且更隐蔽：
///    `xml_escape` 只转义五个元字符时，标题里的 U+0000–U+0008、U+000B、
///    U+000C、U+000E–U+001F、U+FFFE、U+FFFF 仍然是**非法 XML 字符**，
///    `LoadXml` 会抛 0xC00CE508。而调度器在调用本模块**之前**就已经写了
///    `last_fired_at`，因此这条提醒不会被重试、也不会进入"错过补发"——
///    用户永远看不到它，且没有任何提示。
///    这些字符可以经"导入备份"稳定带入（导入不校验标题），
///    也可以经粘贴带入（例如从终端复制的 U+001B 转义序列）。
///
/// 制表符、换行、回车是 XML 允许的，必须保留 —— 通知正文本来就用换行分行。
///
/// # 关于脚本注入
///
/// 本函数不负责 PowerShell 转义，因为内容被放进 here-string（`@'...'@`），
/// 那个形式下 PowerShell 不做任何展开，`$(...)` 与反引号都是字面量。
///
/// **但这里有一个隐性的依赖，必须写下来以免后人踩坑**：
/// 兜住 here-string 不被提前闭合的，正是本函数把 `'` 转成了 `&apos;`
/// （闭合序列需要字面 `'@`）。也就是说，如果有人为了"精简"而去掉单引号转义，
/// here-string 就会可被闭合，立刻变成真实的脚本注入面。
/// 因此单引号**必须**继续转义 —— 它对 XML 不是必需的，对安全是必需的。
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        // 先剔除 XML 非法字符（保留 \t \n \r）
        if is_xml_illegal(c) {
            // 用空格替代而不是直接删除：直接删可能把两个词粘在一起，
            // 让标题变得难以理解；替换成空格至少语义不变。
            out.push(' ');
            continue;
        }

        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // 见上方说明：这一条同时是"防 here-string 被闭合"的关键
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// 是否是 XML 1.0 不允许的字面字符。
///
/// 依据 XML 1.0 的生产式 `Char`：允许 `#x9 | #xA | #xD | [#x20-#xD7FF] |
/// [#xE000-#xFFFD] | [#x10000-#x10FFFF]`。因此非法的是 C0 控制符
/// （除 \t\n\r）、U+FFFE、U+FFFF，以及低于 U+0020 的其它字符。
fn is_xml_illegal(c: char) -> bool {
    let u = c as u32;
    match u {
        0x09 | 0x0A | 0x0D => false, // XML 允许的空白
        0x00..=0x1F => true,         // 其余 C0 控制符
        0xFFFE | 0xFFFF => true,     // 非字符
        _ => false,
    }
}

/// 把触发时刻渲染成中文直觉表达，例如「今天 14:30」。
///
/// 不复用前端的 `datetime.ts`：那在 WebView 里，而通知由 Rust 直接弹出。
/// 两边都需要这个格式，因此各自实现 —— 这也是为什么这个格式必须保持简单，
/// 越复杂的格式化逻辑越容易在两边产生分歧。
fn format_time_label(at: DateTime<FixedOffset>) -> String {
    let local = at.with_timezone(&Local);
    let now = Local::now();

    // 按本地日历天计算差值，而不是按小时数 —— 否则晚上 23:00 看次日 01:00
    // 会算成"今天"，而它显然是明天。
    let days = local
        .date_naive()
        .signed_duration_since(now.date_naive())
        .num_days();

    let clock = local.format("%H:%M").to_string();
    match days {
        0 => format!("今天 {clock}"),
        1 => format!("明天 {clock}"),
        2 => format!("后天 {clock}"),
        -1 => format!("昨天 {clock}"),
        _ => format!("{} {}", local.format("%m月%d日"), clock),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_metacharacters() {
        assert_eq!(xml_escape("a&b"), "a&amp;b");
        assert_eq!(xml_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(xml_escape("say \"hi\""), "say &quot;hi&quot;");
        assert_eq!(xml_escape("it's"), "it&apos;s");
    }

    /// 未转义的 `&` 或 `<` 会让 toast 的 XML 解析失败，
    /// 表现为"只有含特殊字符的标题弹不出通知"。
    #[test]
    fn escaping_prevents_broken_xml() {
        let title = "A & B < C";
        let escaped = xml_escape(title);
        assert!(!escaped.contains('&') || escaped.contains("&amp;"));
        assert!(!escaped.contains('<'));
    }

    /// 中文、emoji 与换行不应被破坏。
    #[test]
    fn preserves_non_ascii_and_newlines() {
        let escaped = xml_escape("交房租\n今天 15:00 🎉");
        assert!(escaped.contains("交房租"));
        assert!(escaped.contains('\n'));
        assert!(escaped.contains("🎉"));
    }

    /// 时间标签必须按日历天判断"今天/明天"，不能按小时数。
    #[test]
    fn time_label_uses_calendar_days() {
        let now = Local::now();
        let label = format_time_label(now.fixed_offset());
        assert!(label.starts_with("今天"), "当前时刻应标为今天：{label}");
    }

    /// XML 1.0 不允许的控制字符必须被剔除。
    ///
    /// 这条对应一个真实的静默失效：含这些字符的标题会让 `LoadXml` 抛异常，
    /// 而调度器**在此之前**已写入 `last_fired_at`，因此不会重试也不会补发 ——
    /// 用户永远看不到那条提醒，且没有任何提示。
    #[test]
    fn strips_xml_illegal_control_characters() {
        // 全部非法：NUL、BEL、VT、FF、ESC、US、非字符
        for bad in [
            '\u{0000}', '\u{0007}', '\u{000B}', '\u{000C}', '\u{001B}', '\u{001F}', '\u{FFFE}',
            '\u{FFFF}',
        ] {
            let escaped = xml_escape(&format!("a{bad}b"));
            assert!(
                !escaped.contains(bad),
                "U+{:04X} 是 XML 非法字符，必须被剔除",
                bad as u32
            );
            assert!(
                escaped.contains('a') && escaped.contains('b'),
                "其它字符应保留"
            );
        }
    }

    /// 制表符、换行、回车是 XML 允许的，必须保留 —— 通知正文靠换行分行。
    #[test]
    fn keeps_xml_allowed_whitespace() {
        let escaped = xml_escape("第一行\n第二行\t带制表符\r结束");
        assert!(escaped.contains('\n'), "换行必须保留");
        assert!(escaped.contains('\t'), "制表符必须保留");
        assert!(escaped.contains('\r'), "回车必须保留");
    }

    /// 单引号必须继续转义 —— 它对 XML 不是必需的，但它是
    /// "here-string 不被提前闭合"的唯一防线。去掉它会立刻变成脚本注入面。
    #[test]
    fn single_quote_escaped_is_load_bearing_for_security() {
        let escaped = xml_escape("'@ Write-Output INJECTED");
        assert!(
            !escaped.contains('\''),
            "单引号必须被转义，否则 here-string 可被 `'@` 提前闭合"
        );
    }

    /// 控制字符用空格替代而不是直接删除，避免把两个词粘在一起。
    #[test]
    fn illegal_chars_become_space_not_deleted() {
        let escaped = xml_escape("abc\u{0000}def");
        assert!(
            escaped.contains("abc def"),
            "非法字符应替换为空格而不是删除，实际：{escaped:?}"
        );
    }
}
