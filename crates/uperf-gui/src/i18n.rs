//! Lightweight built-in GUI localization.
//!
//! The daemon protocol remains language-neutral. The desktop client translates
//! presentation strings and keeps one small per-user language preference.

use std::{
    fs, io,
    sync::atomic::{AtomicU8, Ordering},
};

use gtk::glib;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    #[default]
    English = 0,
    SimplifiedChinese = 1,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LanguageChoice {
    #[default]
    System = 0,
    English = 1,
    SimplifiedChinese = 2,
}

static ACTIVE_LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);
static LANGUAGE_CHOICE: AtomicU8 = AtomicU8::new(LanguageChoice::System as u8);

impl LanguageChoice {
    pub const fn from_index(index: u32) -> Option<Self> {
        match index {
            0 => Some(Self::System),
            1 => Some(Self::English),
            2 => Some(Self::SimplifiedChinese),
            _ => None,
        }
    }

    pub const fn index(self) -> u32 {
        self as u32
    }

    const fn config_value(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
        }
    }

    fn resolve(self) -> Language {
        match self {
            Self::English => Language::English,
            Self::SimplifiedChinese => Language::SimplifiedChinese,
            Self::System => {
                resolve_system_language(glib::language_names().iter().map(glib::GString::as_str))
            }
        }
    }
}

pub fn initialize() {
    let choice = load_choice();
    LANGUAGE_CHOICE.store(choice as u8, Ordering::Relaxed);
    ACTIVE_LANGUAGE.store(choice.resolve() as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    match ACTIVE_LANGUAGE.load(Ordering::Relaxed) {
        1 => Language::SimplifiedChinese,
        _ => Language::English,
    }
}

pub fn language_choice() -> LanguageChoice {
    match LANGUAGE_CHOICE.load(Ordering::Relaxed) {
        1 => LanguageChoice::English,
        2 => LanguageChoice::SimplifiedChinese,
        _ => LanguageChoice::System,
    }
}

pub fn save_language_choice(choice: LanguageChoice) -> io::Result<()> {
    let path = preference_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, choice.config_value())?;
    LANGUAGE_CHOICE.store(choice as u8, Ordering::Relaxed);
    Ok(())
}

fn load_choice() -> LanguageChoice {
    let Ok(value) = fs::read_to_string(preference_path()) else {
        return LanguageChoice::System;
    };
    match value.trim() {
        "en" => LanguageChoice::English,
        "zh-CN" => LanguageChoice::SimplifiedChinese,
        _ => LanguageChoice::System,
    }
}

fn preference_path() -> std::path::PathBuf {
    glib::user_config_dir()
        .join("uperf-linux")
        .join("gui-language")
}

fn resolve_system_language<'a>(names: impl IntoIterator<Item = &'a str>) -> Language {
    if names.into_iter().any(|name| {
        name.eq_ignore_ascii_case("zh")
            || name
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("zh_"))
            || name
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("zh-"))
    }) {
        Language::SimplifiedChinese
    } else {
        Language::English
    }
}

pub fn tr(english: &'static str) -> &'static str {
    translate(language(), english)
}

#[allow(clippy::too_many_lines)]
pub fn translate(language: Language, english: &'static str) -> &'static str {
    if language == Language::English {
        return english;
    }
    match english {
        "Active" => "已启用",
        "Active workload" => "活动工作负载",
        "Add a rule below to pin a mode for a matching process." => {
            "在下方添加规则，为匹配的进程固定运行模式。"
        }
        "Add rule" => "添加规则",
        "Apply" | "Apps" => "应用",
        "Apply privileged frequency limits?" => "应用特权频率限制？",
        "Apply…" => "应用…",
        "Application rules" => "应用规则",
        "Broad game and compatibility-layer matches; detection alone never changes the active mode" => {
            "广泛匹配游戏和兼容层；仅检测到进程不会改变当前模式"
        }
        "Cancel" => "取消",
        "Class" => "类别",
        "Clear" => "清除",
        "Clear active workload" => "清除活动工作负载",
        "Clear explicit workload" => "清除显式工作负载",
        "Clear focused workload" => "清除焦点工作负载",
        "Cluster frequency" => "集群频率",
        "Configuration reload" => "重新加载配置",
        "Connected" => "已连接",
        "Connecting…" => "正在连接…",
        "Connection" => "连接",
        "CPU utilization" => "CPU 使用率",
        "Daemon configuration" => "守护进程配置",
        "Dashboard" => "概览",
        "Detected running workloads" => "检测到的运行中工作负载",
        "Disabled by policy" => "已被策略禁用",
        "Disconnected" => "已断开",
        "Dominant scene" => "主导场景",
        "Edit the config with administrator privileges, then reload it here." => {
            "使用管理员权限编辑配置后，在此重新加载。"
        }
        "Effective profile" => "当前生效配置",
        "Enable & Start" => "启用并启动",
        "Enable rule" => "启用规则",
        "Enabling…" => "正在启用…",
        "English" => "英语",
        "Enter a PID; the daemon resolves and verifies its start time and UID" => {
            "输入 PID；守护进程会解析并验证其启动时间和 UID"
        }
        "Executable path (optional)" => "可执行文件路径（可选）",
        "Follow system language" => "跟随系统语言",
        "Frequency" => "频率",
        "Health" => "健康状态",
        "Health issues" => "健康状态详情",
        "Detailed daemon findings, including informational reports" => {
            "守护进程报告的完整详情，包括提示信息"
        }
        "Incompatible API" => "API 不兼容",
        "Invalid request" => "无效请求",
        "Language" => "语言",
        "Language changes take effect after restarting the application" => {
            "语言更改会在重新打开应用后生效"
        }
        "Language saved. Restart Uperf Linux to apply it." => {
            "语言设置已保存，重新打开 Uperf Linux 后生效。"
        }
        "Lifecycle" => "生命周期",
        "Launch a game, Wine/Proton application, emulator, or Steam process." => {
            "请启动游戏、Wine/Proton 应用、模拟器或 Steam 进程。"
        }
        "Logs" => "日志",
        "Manual bounds are transactional, read back by the daemon, and constrained by thermal safety" => {
            "手动频率范围以事务方式应用并由守护进程回读，同时受温控安全限制"
        }
        "Manual frequency override" => "手动频率覆盖",
        "Match by executable path, process-name regex, or both" => {
            "按可执行文件路径、进程名正则表达式或两者共同匹配"
        }
        "Maximum frequency" => "最高频率",
        "Minimum frequency" => "最低频率",
        "Mode" => "模式",
        "Modes are advertised by the running daemon" => "模式由正在运行的守护进程提供",
        "No active workload" => "没有活动工作负载",
        "No applied rule" => "没有已应用规则",
        "No application rules" => "没有应用规则",
        "No dedicated unit selected" => "未选择专用 unit",
        "No fresh state" => "没有最新状态",
        "No matching processes" => "没有匹配的进程",
        "No overridable targets" => "没有可覆盖的目标",
        "None" => "无",
        "Not authorized" => "未授权",
        "Observed state reported by org.uperflinux.Daemon1" => {
            "org.uperflinux.Daemon1 报告的实际状态"
        }
        "Pending or no matching scheduler rule" => "等待应用或没有匹配的调度规则",
        "Per-CPU load reported by daemon telemetry" => "守护进程遥测报告的逐 CPU 负载",
        "Persistent global rules that pin a mode for matching processes" => {
            "为匹配进程固定运行模式的持久全局规则"
        }
        "Power mode" => "电源模式",
        "Press Refresh to load the latest uperf-linux.service journal." => {
            "点击“刷新”加载最新的 uperf-linux.service 日志。"
        }
        "Process-name regex (optional)" => "进程名正则表达式（可选）",
        "Refresh" => "刷新",
        "Release all" => "全部释放",
        "Reload" => "重新加载",
        "Remove rule" => "删除规则",
        "Request rejected" => "请求被拒绝",
        "Rule" => "规则",
        "Safety state is authoritative; manual settings cannot bypass it" => {
            "安全状态具有最高优先级，手动设置无法绕过"
        }
        "Selection" => "当前选择",
        "Service activation was cancelled or denied" => "服务启用已取消或被拒绝",
        "Service journal" => "服务日志",
        "Service started and enabled for boot" => "服务已启动，并设置为开机自动启动",
        "Set active workload" => "设置活动工作负载",
        "Settings" => "设置",
        "Simplified Chinese" => "简体中文",
        "Start at boot and connect the GUI to the privileged daemon" => {
            "设置开机自动启动，并将 GUI 连接到特权守护进程"
        }
        "State unavailable · not managed" => "状态不可用 · 未托管",
        "Status" => "状态",
        "Source" => "来源",
        "System service" => "系统服务",
        "Systemd cgroup" => "Systemd cgroup",
        "Task scheduler" => "任务调度",
        "Temperature" => "温度",
        "Thermal and hardware limits remain authoritative." => "温控与硬件限制仍具有最高优先级。",
        "Thermal safety" => "温控安全",
        "Unable to read journal" => "无法读取日志",
        "Unable to request service activation" => "无法请求启用服务",
        "Unable to save the language preference" => "无法保存语言设置",
        "Unable to start journalctl" => "无法启动 journalctl",
        "Unavailable" => "不可用",
        "Use" => "使用",
        "Workload PID" => "工作负载 PID",
        "any process" => "任意进程",
        "applied" => "已应用",
        "daemon disconnected before the command was sent" => "命令发送前守护进程已断开",
        "explicit active workload" => "显式活动工作负载",
        "matched" => "匹配",
        "ms" => "毫秒",
        "no dedicated unit" => "没有专用 unit",
        "not applied" => "未应用",
        "not managed" => "未托管",
        "observed" => "已观测",
        "override" => "覆盖",
        "priority" => "优先级",
        "retrying in" => "将在以下时间后重试",
        "s" => "秒",
        "safety cap active" => "安全上限已生效",
        "sensor data stale" => "传感器数据已过期",
        "stale" => "已过期",
        "tasks applied" => "个任务已应用",
        "(journal is empty)" => "（日志为空）",
        "Sensors healthy" => "传感器正常",
        _ => english,
    }
}

pub fn translate_known(message: &str) -> String {
    let known = match message {
        "PID must be a positive integer" => "PID 必须是正整数",
        "PID must be non-zero" => "PID 不能为零",
        "Provide an executable path or a process-name regex" => {
            "请提供可执行文件路径或进程名正则表达式"
        }
        "Select a maximum frequency" => "请选择最高频率",
        "Select a minimum frequency" => "请选择最低频率",
        "Select a mode for the rule" => "请为规则选择模式",
        "minimum frequency exceeds maximum frequency" => "最低频率高于最高频率",
        "requested frequencies exceed the advertised hardware bounds" => {
            "请求的频率超出硬件公布范围"
        }
        "requested frequency is not an advertised operating point" => {
            "请求的频率不是硬件公布的工作点"
        }
        _ => return message.to_owned(),
    };
    if language() == Language::SimplifiedChinese {
        known.to_owned()
    } else {
        message.to_owned()
    }
}

pub fn localized_mode_label(id: &str, fallback: &str) -> String {
    if language() != Language::SimplifiedChinese {
        return fallback.to_owned();
    }
    match id {
        "auto" => "自动".to_owned(),
        "powersave" => "省电".to_owned(),
        "balance" | "balanced" => "均衡".to_owned(),
        "performance" => "性能".to_owned(),
        _ => fallback.to_owned(),
    }
}

pub fn localized_mode_description(id: &str, fallback: &str) -> String {
    if language() != Language::SimplifiedChinese {
        return fallback.to_owned();
    }
    match id {
        "auto" => "使用活动工作负载规则，否则使用均衡默认模式".to_owned(),
        "powersave" => "优先使用高能效工作点".to_owned(),
        "balance" | "balanced" => "兼顾响应速度和能效".to_owned(),
        "performance" => "在安全上限内优先保证响应速度".to_owned(),
        _ => fallback.to_owned(),
    }
}

pub fn localized_protocol_value(value: &str) -> String {
    if language() != Language::SimplifiedChinese {
        return value.to_owned();
    }
    match value {
        "active" => "活动".to_owned(),
        "auto" => "自动".to_owned(),
        "balance" | "balanced" => "均衡".to_owned(),
        "boost" | "Boost" => "加速".to_owned(),
        "connected" => "已连接".to_owned(),
        "critical" => "严重".to_owned(),
        "degraded" => "降级".to_owned(),
        "error" => "错误".to_owned(),
        "explicit" => "显式指定".to_owned(),
        "focus" => "窗口焦点".to_owned(),
        "gesture" | "Gesture" => "手势".to_owned(),
        "healthy" => "健康".to_owned(),
        "info" => "提示".to_owned(),
        "idle" | "Idle" => "空闲".to_owned(),
        "normal" => "正常".to_owned(),
        "performance" => "性能".to_owned(),
        "powersave" => "省电".to_owned(),
        "running" => "运行中".to_owned(),
        "stale" => "已过期".to_owned(),
        "switch" | "Switch" => "切换".to_owned(),
        "thermal-degraded" => "温控降级".to_owned(),
        "throttled" => "已限频".to_owned(),
        "touch" | "Touch" => "触摸".to_owned(),
        "trigger" | "Trigger" => "触发".to_owned(),
        "unavailable" => "不可用".to_owned(),
        "wake" | "Wake" => "唤醒".to_owned(),
        "warning" => "警告".to_owned(),
        "all mandatory components are healthy" => "所有必要组件均正常".to_owned(),
        "one or more mandatory components failed" => "一个或多个必要组件发生故障".to_owned(),
        "running with one or more safety or capability restrictions" => {
            "正在受一个或多个安全或功能限制运行".to_owned()
        }
        _ => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Language, LanguageChoice, resolve_system_language, translate};

    #[test]
    fn language_choice_indices_are_stable() {
        assert_eq!(LanguageChoice::from_index(0), Some(LanguageChoice::System));
        assert_eq!(
            LanguageChoice::from_index(2),
            Some(LanguageChoice::SimplifiedChinese)
        );
        assert_eq!(LanguageChoice::from_index(3), None);
    }

    #[test]
    fn system_language_recognizes_common_chinese_locale_forms() {
        assert_eq!(
            resolve_system_language(["zh_CN.UTF-8", "zh", "en"].into_iter()),
            Language::SimplifiedChinese
        );
        assert_eq!(
            resolve_system_language(["en_US.UTF-8", "en"].into_iter()),
            Language::English
        );
    }

    #[test]
    fn chinese_catalog_has_an_english_fallback() {
        assert_eq!(translate(Language::SimplifiedChinese, "Dashboard"), "概览");
        assert_eq!(
            translate(Language::SimplifiedChinese, "untranslated protocol value"),
            "untranslated protocol value"
        );
    }
}
