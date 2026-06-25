// Python 自动化测试「脚本运行器」窗口的事件接线 + Python 版本检测/校验/启动。
// 通过 include! 进 main.rs 的 crate-root 模块，共享其 import 与私有项（无单独 use）。
//
// 版本切换 = 改 interp 路径字符串；每次 Run 现读现校验，零 rebuild、零 pip。
// 用户脚本由所选 python.exe **直接运行**（python 用户脚本.py），pcanwork.py 作为
// 纯 stdlib 库经 PYTHONPATH(=PCANWORK_CLIENT_DIR) 提供。

/// 运行一个外部命令并捕获 stdout+stderr，带硬超时（不弹控制台窗口）。
fn run_capture(exe: &str, args: &[&str], timeout: std::time::Duration) -> Option<(bool, String)> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), s))
}

/// 从一行文本里提取最后一个 Windows 路径（`X:\...`）。
fn extract_win_path(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 3 {
        return None;
    }
    for i in (0..b.len() - 2).rev() {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/') {
            return Some(s[i..].trim().to_string());
        }
    }
    None
}

/// 扫描目录下的 python.exe（dir 本身 + 一层子目录，覆盖 PythonXX / pyenv 版本目录）。
fn scan_python_dir(dir: &str, out: &mut Vec<String>) {
    let base = std::path::Path::new(dir);
    let direct = base.join("python.exe");
    if direct.exists() {
        out.push(direct.to_string_lossy().to_string());
    }
    if let Ok(rd) = std::fs::read_dir(base) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let exe = p.join("python.exe");
                if exe.exists() {
                    out.push(exe.to_string_lossy().to_string());
                }
            }
        }
    }
}

/// 检测已装 Python：以 `py -0p` 为权威源 + where + 常见目录；丢弃 WindowsApps 假 stub；
/// 逐个 `-c print(version)` 带 3s 超时校验，仅接受可运行的 3.7+；规范化路径去重。
/// 返回 (exe 路径, "Python X.Y")。
fn detect_pythons() -> Vec<(String, String)> {
    let mut candidates: Vec<String> = Vec::new();
    // 1) py 启动器（最权威，列出所有已注册版本）
    if let Some((_, out)) = run_capture("py", &["-0p"], std::time::Duration::from_secs(3)) {
        for line in out.lines() {
            if let Some(p) = extract_win_path(line) {
                candidates.push(p);
            }
        }
    }
    // 2) where python / python3（仅作候选）
    for w in ["python", "python3"] {
        if let Some((_, out)) = run_capture("where", &[w], std::time::Duration::from_secs(3)) {
            for line in out.lines() {
                let p = line.trim();
                if p.to_ascii_lowercase().ends_with("python.exe") {
                    candidates.push(p.to_string());
                }
            }
        }
    }
    // 3) 常见安装目录
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        scan_python_dir(&format!("{la}\\Programs\\Python"), &mut candidates);
    }
    scan_python_dir("C:\\", &mut candidates); // C:\PythonXX
    if let Ok(up) = std::env::var("USERPROFILE") {
        scan_python_dir(&format!("{up}\\.pyenv\\pyenv-win\\versions"), &mut candidates);
    }
    if let Ok(cp) = std::env::var("CONDA_PREFIX") {
        scan_python_dir(&cp, &mut candidates);
    }

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for c in candidates {
        if c.to_ascii_lowercase().contains("\\windowsapps\\") {
            continue; // Microsoft Store 应用执行别名 stub：非可用解释器
        }
        let canon = std::fs::canonicalize(&c).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| c.clone());
        if !seen.insert(canon) {
            continue;
        }
        if let Some((true, out)) = run_capture(&c, &["-c", "import sys;print(sys.version_info[0],sys.version_info[1])"], std::time::Duration::from_secs(3)) {
            let nums: Vec<u32> = out.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if nums.len() >= 2 && nums[0] == 3 && nums[1] >= 7 {
                result.push((c.clone(), format!("Python {}.{}", nums[0], nums[1])));
            }
        }
    }
    result
}

/// 用与运行相同的环境探测：版本 >= 3.7、stdlib 齐全、能 `import pcanwork`。失败给出明确中文原因。
fn validate_interp(exe: &str, client_dir: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    if exe.trim().is_empty() {
        return Err("未指定 Python 解释器".into());
    }
    let probe = "import sys,socket,json,dataclasses,queue,threading,runpy; assert sys.version_info>=(3,7); import pcanwork";
    let mut child = Command::new(exe)
        .args(["-c", probe])
        .env("PYTHONPATH", client_dir)
        .env("PYTHONUTF8", "1")
        .creation_flags(0x0800_0000)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("无法启动解释器: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                if st.success() {
                    return Ok(());
                }
                let err = child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stderr).trim().lines().last().unwrap_or("").to_string()).unwrap_or_default();
                return Err(format!("解释器不可用（需 3.7+ 且能 import pcanwork）: {err}"));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err("解释器校验超时".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(format!("校验出错: {e}")),
        }
    }
}

/// pcanwork.py 所在目录（与 exe 同目录）。
fn client_dir() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// 检测并填充 Python 下拉（合并已持久化的自定义路径，避免被检测清掉）。
fn populate_pythons(w: &ScriptRunnerWindow, persisted: &str) {
    let found = detect_pythons();
    let mut labels: Vec<slint::SharedString> = found.iter().map(|(p, ver)| format!("{ver} | {p}").into()).collect();
    if !persisted.trim().is_empty() && !found.iter().any(|(p, _)| p == persisted) {
        labels.push(format!("自定义 | {persisted}").into());
    }
    w.set_python_list(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(labels))));
}

#[allow(clippy::too_many_arguments)]
/// 启动一个 Python 进程（脚本或套件驱动），带 IPC 环境 + 可选附加环境变量，
/// reader 线程把 stdout/stderr 喂进 App.py_output。校验失败/启动失败返回 false。
/// timeout_secs = 看门狗超时（单脚本 120；套件给大值兜底，逐测试超时由驱动自管）。
fn spawn_py_process(
    app: &Rc<std::cell::RefCell<App>>,
    w: &ScriptRunnerWindow,
    interp: &str,
    script: &str,
    extra_env: &[(&str, &str)],
    ipc_port: u16,
    token: &str,
    timeout_secs: u64,
) -> bool {
    let cdir = client_dir();
    if let Err(e) = validate_interp(interp, &cdir) {
        w.set_status_text(e.into());
        w.set_result(-1);
        return false;
    }
    let header = format!("运行中…（完整日志: {}）\n\n", run_log_path().display());
    {
        let mut a = app.borrow_mut();
        a.python_interpreter = interp.to_string();
        a.py_stop_flag = false;
        a.py_started = Some(std::time::Instant::now());
        a.py_timeout_secs = timeout_secs;
        a.run_status = "Running".into();
        a.py_output = header.clone();
        a.py_dirty = false; // 下方直接 set_output，避免重复
    }
    w.set_output(header.into());
    w.set_running(true);
    w.set_result(0);
    w.set_status_text("运行中…".into());

    let pythonpath = match std::env::var("PYTHONPATH") {
        Ok(ev) if !ev.is_empty() => format!("{cdir};{ev}"),
        _ => cdir.clone(),
    };
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(interp);
    cmd.arg(script)
        .env("PCANWORK_IPC_PORT", ipc_port.to_string())
        .env("PCANWORK_IPC_TOKEN", token)
        .env("PCANWORK_CLIENT_DIR", &cdir)
        .env("PYTHONPATH", &pythonpath)
        .env("PYTHONUTF8", "1")
        .env("PYTHONUNBUFFERED", "1"); // 实时流式输出（含套件子进程，保证日志按序）
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = match cmd
        .creation_flags(0x0800_0000)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            w.set_status_text(format!("启动失败: {e}").into());
            w.set_result(-1);
            w.set_running(false);
            app.borrow_mut().py_started = None;
            return false;
        }
    };
    // reader 线程：只持有 mpsc::Sender<String>（Send 安全），不捕获 App。
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(format!("[stderr] {line}")).is_err() {
                    break;
                }
            }
        });
    }
    let mut a = app.borrow_mut();
    a.py_out_rx = Some(rx);
    a.py_child = Some(child);
    true
}

fn wire_pyauto(app: Rc<std::cell::RefCell<App>>, ui: &AppWindow, script_runner_window: &ScriptRunnerWindow, ipc_port: u16, ipc_token: String) {
    // 打开窗口：从持久化字段回填 + 检测一次 + 显示。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        ui.on_open_script_runner(move || {
            let Some(w) = rw.upgrade() else { return };
            {
                let a = app.borrow();
                if !a.python_interpreter.is_empty() {
                    w.set_interp_path(a.python_interpreter.clone().into());
                }
                if !a.last_script_path.is_empty() {
                    w.set_script_path(a.last_script_path.clone().into());
                }
                // 回显上次运行的输出/状态（重新打开窗口时不丢报错日志）
                if !a.py_output.is_empty() {
                    w.set_output(a.py_output.clone().into());
                    w.set_running(a.py_child.is_some());
                    let rs = a.run_status.clone();
                    w.set_result(if rs.starts_with("PASS") {
                        1
                    } else if rs.starts_with("FAIL") {
                        -1
                    } else {
                        0
                    });
                }
            }
            let persisted = app.borrow().python_interpreter.clone();
            populate_pythons(&w, &persisted);
            if app.borrow().py_output.is_empty() {
                w.set_status_text("就绪".into());
            }
            let _ = w.show();
        });
    }

    // 检测已装版本。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        script_runner_window.on_detect_pythons(move || {
            let Some(w) = rw.upgrade() else { return };
            w.set_status_text("检测中…".into());
            let persisted = app.borrow().python_interpreter.clone();
            populate_pythons(&w, &persisted);
            w.set_status_text("检测完成".into());
        });
    }

    // 从下拉选择（标签格式 "Python X.Y | 路径"）→ 回填真实路径并持久化。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        script_runner_window.on_python_selected(move |label| {
            let path = label.as_str().rsplit(" | ").next().unwrap_or("").trim().to_string();
            if path.is_empty() {
                return;
            }
            if let Some(w) = rw.upgrade() {
                w.set_interp_path(path.clone().into());
            }
            app.borrow_mut().python_interpreter = path;
        });
    }

    // 浏览解释器 exe（异步对话框，避免 UI 线程嵌套模态死锁）。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        script_runner_window.on_browse_interp(move || {
            let app = app.clone();
            let rw = rw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = rw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new().add_filter("Python", &["exe"]);
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(file) = dlg.pick_file().await else { return };
                let p = file.path().to_string_lossy().to_string();
                w.set_interp_path(p.clone().into());
                match validate_interp(&p, &client_dir()) {
                    Ok(()) => {
                        w.set_status_text("解释器可用".into());
                        w.set_result(0);
                    }
                    Err(e) => {
                        w.set_status_text(e.into());
                        w.set_result(-1);
                    }
                }
                app.borrow_mut().python_interpreter = p;
            });
        });
    }

    // 浏览测试脚本 .py。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        script_runner_window.on_browse_script(move || {
            let app = app.clone();
            let rw = rw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = rw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new().add_filter("Python 脚本", &["py"]);
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(file) = dlg.pick_file().await else { return };
                let p = file.path().to_string_lossy().to_string();
                w.set_script_path(p.clone().into());
                app.borrow_mut().last_script_path = p;
            });
        });
    }

    // 运行单脚本：校验解释器 → 启动用户脚本（带 IPC 环境变量）→ reader 线程喂输出。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        let token = ipc_token.clone();
        script_runner_window.on_run_script(move || {
            let Some(w) = rw.upgrade() else { return };
            let interp = {
                let p = w.get_interp_path().to_string();
                if !p.trim().is_empty() {
                    p
                } else {
                    app.borrow().python_interpreter.clone()
                }
            };
            let script = w.get_script_path().to_string();
            if script.trim().is_empty() {
                w.set_status_text("请先选择测试脚本".into());
                w.set_result(-1);
                return;
            }
            app.borrow_mut().last_script_path = script.clone();
            spawn_py_process(&app, &w, &interp, &script, &[], ipc_port, &token, 120);
        });
    }

    // 运行套件：选文件夹 → 跑内置 run_suite.py 批量执行其中所有 .py，汇总结果。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        let token = ipc_token.clone();
        script_runner_window.on_run_suite(move || {
            let app = app.clone();
            let rw = rw.clone();
            let token = token.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = rw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new();
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(folder) = dlg.pick_folder().await else { return };
                let dir = folder.path().to_string_lossy().to_string();
                let interp = {
                    let p = w.get_interp_path().to_string();
                    if !p.trim().is_empty() {
                        p
                    } else {
                        app.borrow().python_interpreter.clone()
                    }
                };
                let suite = format!("{}/templates/run_suite.py", client_dir());
                // 套件总超时给大值（逐测试超时由 run_suite.py 自管，默认每个 120s）。
                spawn_py_process(&app, &w, &interp, &suite, &[("PCANWORK_SUITE_DIR", dir.as_str())], ipc_port, &token, 3600);
            });
        });
    }

    // 停止：仅置标志，由 tick 的 reap_child 在 UI 线程 kill（集中管理子进程）。
    {
        let app = app.clone();
        let rw = script_runner_window.as_weak();
        script_runner_window.on_stop_run(move || {
            app.borrow_mut().py_stop_flag = true;
            if let Some(w) = rw.upgrade() {
                w.set_running(false);
                w.set_status_text("已请求停止".into());
            }
        });
    }
}
