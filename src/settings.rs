//! 配置持久化：退出时保存、启动时恢复（通道/主题/缓存/过滤/布局/DBC 路径）。
//! 存于 exe 同目录的 pcanwork_settings.json（纯文件，无需硬件）。

use crate::can::DeviceConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub channels: Vec<DeviceConfig>,
    #[serde(default)]
    pub channel_sel: i32,
    #[serde(default)]
    pub dark: bool,
    #[serde(default = "default_true")]
    pub big: bool,
    #[serde(default)]
    pub trace_cap: usize,
    #[serde(default)]
    pub chart_cap: usize,
    #[serde(default)]
    pub f_id: String,
    #[serde(default)]
    pub f_name: String,
    #[serde(default)]
    pub f_data: String,
    #[serde(default)]
    pub dir_filter: i32,
    #[serde(default)]
    pub dbc_path: Option<String>, // 旧版单 DBC 字段(向后兼容读取)
    #[serde(default)]
    pub dbc_paths: Vec<String>, // 多 DBC 路径列表
    #[serde(default)]
    pub left_w: f32,
    #[serde(default)]
    pub bottom_h: f32,
    #[serde(default)]
    pub mode_trace: bool,
    #[serde(default)]
    pub time_mode: i32,
    #[serde(default)]
    pub cols_hidden: String, // 隐藏的报文表列, 逗号分隔的列 key
    #[serde(default)]
    pub sim_widgets: String, // 仿真面板控件(JSON 序列化)
    #[serde(default)]
    pub lang_en: bool, // 界面语言: true=英文
    #[serde(default)]
    pub python_interpreter_path: String, // Python 自动化：选定的解释器 exe 路径
    #[serde(default)]
    pub last_script_path: String, // Python 自动化：上次运行的脚本路径
    #[serde(default)]
    pub expr_vars: Vec<crate::ExprVar>, // 表达式派生信号(名/公式/单位)
    #[serde(default)]
    pub console_enabled: bool, // CAN 报文日志: 是否捕获
    #[serde(default = "neg_one")]
    pub console_id: i64, // CAN 报文日志: 捕获的 ID(-1=任意)
    #[serde(default)]
    pub console_ch: i32, // CAN 报文日志: 捕获的通道(0=任意)
    #[serde(default = "default_renderer")]
    pub renderer: String, // 渲染器: "auto"(默认,远程/虚拟显示自动用CPU) | "gpu"(femtovg) | "cpu"(software)
}

pub fn default_renderer() -> String {
    "auto".to_string()
}

fn neg_one() -> i64 {
    -1
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            channels: Vec::new(),
            channel_sel: 0,
            dark: false,
            big: true,
            trace_cap: 0,
            chart_cap: 0,
            f_id: String::new(),
            f_name: String::new(),
            f_data: String::new(),
            dir_filter: 0,
            dbc_path: None,
            dbc_paths: Vec::new(),
            left_w: 0.0,
            bottom_h: 0.0,
            mode_trace: true,
            time_mode: 0,
            cols_hidden: String::new(),
            sim_widgets: String::new(),
            lang_en: false,
            python_interpreter_path: String::new(),
            last_script_path: String::new(),
            expr_vars: Vec::new(),
            console_enabled: false,
            console_id: -1,
            console_ch: 0,
            renderer: default_renderer(),
        }
    }
}

fn settings_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            return dir.join("pcanwork_settings.json");
        }
    PathBuf::from("pcanwork_settings.json")
}

/// 读取上次配置（无文件或解析失败返回 None）。
pub fn load() -> Option<Settings> {
    let s = std::fs::read_to_string(settings_path()).ok()?;
    serde_json::from_str(&s).ok()
}

/// 保存当前配置（失败静默）。
pub fn save(s: &Settings) {
    if let Ok(txt) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(settings_path(), txt);
    }
}
