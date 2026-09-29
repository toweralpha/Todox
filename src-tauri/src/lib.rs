//! Todox —— 苹果风格的 Windows 桌面待办应用。
//!
//! 本文件只负责装配：初始化数据库、注册插件与命令、启动调度器、建立系统托盘。
//! 业务逻辑一律不在这里出现。

pub mod commands;
pub mod db;
pub mod domain;
pub mod nlp;
pub mod notifier;
pub mod repo;
pub mod scheduler;
pub mod sync;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
// `Emitter` 提供 AppHandle::emit。trait 方法必须显式导入才能调用。
use tauri::{Emitter, Manager, WindowEvent};

/// 运行期共享状态中除数据库之外的部分。
///
/// 单独成一个结构体而不是往 db 上挂：数据库句柄的职责是数据访问，
/// 调度器句柄的职责是通知重算，两者没有内聚关系。
pub struct AppRuntime {
    pub scheduler: scheduler::SchedulerHandle,
    /// 是否正在退出。
    ///
    /// 需要它是因为"关窗行为"是从数据库读的（见 [`close_to_tray_enabled`]），
    /// 因此点托盘菜单的「退出」时没法靠改设置来绕过关窗拦截 —— 那样改的是
    /// 持久化配置，会污染用户的设置。用一个纯内存标志表达"这一次是真退出"。
    pub quitting: std::sync::atomic::AtomicBool,
}

pub fn run() {
    // 单实例必须**最先注册**：它的回调在第二个实例启动时触发，
    // 注册晚了会出现两个实例都在初始化的短暂窗口，那正是要避免的。
    //
    // 重复启动时唤起已有窗口而不是开新进程 —— 多个常驻进程会直接
    // 破坏"低内存占用"这个核心指标。
    let builder =
        tauri::Builder::default().plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 复用同一个入口：窗口可能已被销毁（见窗口事件处理器），
            // 此时它会按配置重建，而不是无声地什么都不做。
            show_main_window(app);
        }));

    builder
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // 开机自启不带额外参数：用户希望的是"它在那儿"，而不是"它跳出来"
            None,
        ))
        // 系统原生的文件对话框。用于让用户选择导出到哪里、从哪里导入 ——
        // 自绘文件浏览器或在输入框里手输绝对路径都是明显的体验降级。
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // ---------- 数据库 ----------
            // 放在系统标准的应用数据目录，而不是程序安装目录：
            // 安装目录在打包后可能位于 Program Files 等只读位置，
            // 且卸载/升级时会被覆盖，用户的待办数据必须独立于程序本体。
            let data_dir = app.path().app_data_dir().map_err(|e| {
                format!("无法定位应用数据目录：{e}。Todox 需要可写的用户目录来保存待办数据。")
            })?;

            let db = db::connection::Db::open(&data_dir)
                .map_err(|e| format!("数据库初始化失败：{e}"))?;

            println!("Todox 数据目录：{}", data_dir.display());

            // ---------- 调度器 ----------
            // Db 是 Arc 语义，克隆出的句柄与状态里的是同一个连接。
            // 共享而非各开一个连接，是为了让调度器与应用状态共用同一把写锁，
            // 避免 WAL 下两个连接互相看不到对方的写入。
            let scheduler = scheduler::spawn(db.clone());

            app.manage(db);
            app.manage(AppRuntime {
                scheduler,
                quitting: std::sync::atomic::AtomicBool::new(false),
            });

            // ---------- 系统托盘 ----------
            build_tray(app)?;

            // ---------- 全局快捷键 ----------
            setup_global_shortcut(app);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 关闭窗口时保留在托盘而非退出 —— 这是待办应用的关键行为：
                // 用户关掉窗口只是"看完了"，而不是"不想被提醒了"。
                // 若这里真的退出进程，提醒能力就完全失效了。
                //
                // 点托盘菜单的「退出」时会先立起 quitting 标志走另一条路，
                // 因此这里的判断不会妨碍用户真正退出。
                let quitting = window
                    .app_handle()
                    .try_state::<AppRuntime>()
                    .map(|rt| rt.quitting.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(false);

                let should_hide = !quitting && close_to_tray_enabled(window.app_handle());

                if should_hide {
                    // **先阻止默认行为，再销毁窗口。**
                    //
                    // prevent_close 还有一个关键副作用：它会让 Tauri 在
                    // "最后一个窗口关闭"时不退出应用。我们需要它 —— 因为下面
                    // 确实把唯一的窗口销毁了，而进程必须继续活着（托盘常驻 +
                    // 定时提醒全靠它）。
                    api.prevent_close();

                    // 用 destroy 而不是 hide：hide 只是把窗口藏起来，
                    // WebView2 的渲染、GPU、网络等子进程仍然全部驻留内存
                    // （实测约 320MB）。而待办应用绝大多数时间都待在托盘里，
                    // 那段时间渲染进程完全可以不存在。
                    //
                    // 代价是再次打开窗口时需要重建 WebView，会有一瞬间的延迟
                    // —— 对"关掉窗口"这个动作来说，这个交换非常划算。
                    if let Err(e) = window.destroy() {
                        // 销毁失败时退回隐藏：功能上仍然正确，只是省不下内存。
                        // 绝不能因为释放内存失败就让用户看不见窗口。
                        eprintln!("销毁窗口失败，改为隐藏：{e}");
                        let _ = window.hide();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_task,
            commands::create_task_from_text,
            commands::parse_input,
            commands::list_tasks,
            commands::delete_task,
            commands::restore_task,
            commands::complete_task,
            commands::uncomplete_task,
            commands::task_completions,
            commands::unfinished_count,
            commands::table_counts,
            commands::get_settings,
            commands::save_settings,
            commands::pending_missed,
            commands::acknowledge_missed,
            commands::acknowledge_all_missed,
            commands::snooze_reminder,
            commands::clear_snooze,
            commands::update_task,
            commands::stats_overview,
            commands::stats_daily,
            commands::stats_by_task,
            commands::export_data,
            commands::import_data,
        ])
        .build(tauri::generate_context!())
        .expect("Todox 启动失败")
        .run(|app, event| {
            // 接管"最后一个窗口关闭 → 退出应用"这条默认路径。
            //
            // 为什么必须接管：关窗时我们会**销毁**窗口（为了释放 WebView2 的
            // 数百 MB 内存），而 Tauri 的默认行为是最后一个窗口关闭就退出进程。
            // 实测确认：仅靠 `api.prevent_close()` 拦不住 destroy 之后的退出，
            // 进程真的会消失 —— 那会让托盘常驻与定时提醒全部失效，
            // 是整个应用最核心的能力。
            //
            // 因此这里明确拒绝退出，只在用户从托盘菜单选择「退出」时才放行。
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                let quitting = app
                    .try_state::<AppRuntime>()
                    .map(|rt| rt.quitting.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(false);

                if !quitting {
                    api.prevent_exit();
                }
            }
        });
}

/// 建立系统托盘图标与菜单。
fn build_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let open_item = MenuItem::with_id(app, "open", "打开 Todox", true, None::<&str>)?;
    let float_item = MenuItem::with_id(app, "float", "悬浮窗（快速记录）", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &float_item, &quit_item])?;

    TrayIconBuilder::with_id("main-tray")
        // 用应用自身图标。`default_window_icon` 在打包后由 Tauri 注入，
        // 因此不需要额外附带一份托盘专用图标文件。
        .icon(app.default_window_icon().cloned().ok_or("缺少应用图标")?)
        .tooltip("Todox")
        .menu(&menu)
        // 左键单击托盘图标不弹菜单，而是直接唤出悬浮窗 ——
        // 这是更符合直觉的行为：用户点托盘图标通常是想"快点记一件事"，
        // 而悬浮窗正好是为这个场景设计的。完整界面留给菜单项。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "float" => toggle_float_window(app),
            "quit" => {
                // 用户明确选择退出时才真正退出。
                // 先立起标志，否则下面触发的关闭会被"关窗即保留在托盘"拦下。
                if let Some(rt) = app.try_state::<AppRuntime>() {
                    rt.quitting
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键"抬起"而不是"按下"：按下就响应会让拖拽托盘图标时误触发。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_float_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// 读设置判断关窗时是否应保留在托盘。
///
/// 直接从数据库读，而不是在内存里缓存一份：这个函数每次关窗才调用一次
/// （极低频），而缓存意味着设置改动时要记得同步更新 —— 那类"忘记同步"
/// 的 bug 会让用户在设置里关掉托盘模式却依然关不掉窗口。
///
/// 读不到设置时返回 true：宁可多留一个托盘图标，也不要意外退出应用
/// 而让用户失去所有提醒。
fn close_to_tray_enabled(app: &tauri::AppHandle) -> bool {
    app.try_state::<db::connection::Db>()
        .and_then(|db| repo::settings_repo::SettingsRepo::new(&db).load().ok())
        .map(|s| s.close_to_tray)
        .unwrap_or(true)
}

/// 唤出并聚焦主窗口；若窗口已被销毁则按配置重建。
///
/// 窗口的生命周期在本应用里是"按需存在"的：关窗时会销毁它（释放 WebView2
/// 的数百 MB 内存），因此所有唤出路径 —— 托盘左键、托盘菜单、全局快捷键、
/// 单实例回调 —— 都必须走这里，而不能假设窗口一定还在。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }

    // 窗口不存在 → 重建。
    //
    // 从 `tauri.conf.json` 的窗口配置重建，而不是在代码里再写一遍尺寸、
    // 标题、最小宽高等参数 —— 那样两份配置迟早会脱节，表现为"重建后的窗口
    // 和第一次打开的长得不一样"。
    let config = app.config().app.windows.iter().find(|w| w.label == "main");

    match config {
        Some(cfg) => match tauri::WebviewWindowBuilder::from_config(app, cfg) {
            Ok(builder) => {
                if let Err(e) = builder.build() {
                    eprintln!("重建主窗口失败：{e}");
                }
            }
            Err(e) => eprintln!("无法从配置构造主窗口：{e}"),
        },
        None => eprintln!("配置里找不到 label 为 main 的窗口，无法重建"),
    }
}

/// 唤出或隐藏悬浮窗。
///
/// 用"切换"而不是"显示"：托盘图标与全局快捷键都是同一个动作，
/// 用户按第二次的意图显然是"收起来"。若只做显示，用户就得再去找关闭按钮。
///
/// 悬浮窗与主窗口是**两个独立的窗口**：悬浮窗始终置顶且不占任务栏，
/// 关掉主窗口不影响它，反之亦然。
fn toggle_float_window(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("float") else {
        // 配置里若缺少 float 窗口，这里给出明确提示而不是静默失败
        eprintln!("找不到 label 为 float 的窗口，无法显示悬浮窗");
        return;
    };

    match win.is_visible() {
        Ok(true) => {
            let _ = win.hide();
        }
        _ => {
            // 显示前先居中到当前屏幕。用户可能把主窗口拖到了副屏，
            // 而悬浮窗上次的位置可能已经不在任何可见区域内。
            let _ = win.center();
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

/// 注册全局快捷键。
///
/// 失败**不能**让应用启动失败：用户可能已经把快捷键分配给了别的程序，
/// 那是很常见的情况。此时只提示，其余功能照常可用。
fn setup_global_shortcut(app: &tauri::App) {
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    let handle = app.handle().clone();

    // Ctrl+Alt+Space：唤出**悬浮窗**。
    //
    // 选择悬浮窗而不是主窗口：按这个快捷键的用户意图几乎总是"立刻记一件事"，
    // 而悬浮窗打开即聚焦输入框、回车即保存 —— 全程不必离开手上的工作。
    // 唤出完整界面需要更多点击，反而更慢。
    let float_shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
    let float_handle = handle.clone();
    if let Err(e) =
        app.global_shortcut()
            .on_shortcut(float_shortcut, move |_app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    toggle_float_window(&float_handle);
                }
            })
    {
        eprintln!(
            "注册全局快捷键 Ctrl+Alt+Space（悬浮窗）失败，可能已被其它程序占用：{e}。\
             仍可通过托盘菜单打开悬浮窗。"
        );
    }

    // Ctrl+Alt+N：唤出完整主窗口（N = New，语义上好记）。
    //
    // 曾用 Ctrl+Alt+T，实测在本机已被其它程序占用。选 N 是因为它冲突概率更低，
    // 且语义与"新建任务"对应。注册失败时会在下方打印提示，
    // 不影响托盘菜单这条备用路径。
    let main_shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyN);
    let main_handle = handle.clone();
    if let Err(e) =
        app.global_shortcut()
            .on_shortcut(main_shortcut, move |_app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    show_main_window(&main_handle);
                    // 通知前端聚焦快速添加输入框。
                    //
                    // 窗口的显示与聚焦属于后端能力，而"焦点落到哪个 DOM 元素"
                    // 只有前端知道，因此这里只发信号，具体行为由前端实现。
                    let _ = main_handle.emit("todox://focus-quick-add", ());
                }
            })
    {
        eprintln!("注册全局快捷键 Ctrl+Alt+N（主窗口）失败：{e}。仍可通过托盘菜单打开。");
    }
}
