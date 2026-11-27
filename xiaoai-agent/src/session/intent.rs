//! Local intent router for zero-latency voice commands (wake-only, noise, volume, music, exit).

use crate::audio::VolumeAction;
use crate::tts::{
    classify_exit_command, classify_music_voice_control, is_noise_or_filler, is_wake_word_only,
    strip_punctuation_and_space, strip_wake_prefix, ExitAction, MusicVoiceControl,
};

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalIntent {
    WakeWordOnly,
    EmptyOrNoise,
    Volume(VolumeAction),
    Music(MusicVoiceControl),
    Exit(ExitAction),
    Chat(String),
}

/// 识别中文音量控制指令，严格避免把普通问答（如「音量是什么原理」「声音为什么这么大」）误判为调音量。
#[allow(dead_code)]
pub fn classify_volume_command(text: &str) -> Option<VolumeAction> {
    let cleaned = strip_punctuation_and_space(text);
    if cleaned.is_empty() || cleaned.chars().count() > 20 {
        return None;
    }
    // 排除疑问/科普类语句，防止误伤。
    if ["什么", "为什么", "为何", "怎么", "原理", "含义", "意思", "区别", "多少", "吗"]
        .iter()
        .any(|q| cleaned.contains(q))
    {
        return None;
    }

    let core = cleaned
        .trim_end_matches(|c| matches!(c, '吧' | '啊' | '呀' | '哦' | '呢' | '哈' | '了'))
        .trim();
    if core.is_empty() {
        return None;
    }

    let trimmed = strip_polite_prefix(core);
    if trimmed.is_empty() {
        return None;
    }

    // 1. 静音
    if matches!(
        trimmed,
        "静音"
            | "开启静音"
            | "打开静音"
            | "关闭声音"
            | "关掉声音"
            | "声音关闭"
            | "声音关掉"
            | "声音调到零"
            | "音量调到零"
    ) {
        return Some(VolumeAction::Mute);
    }

    // 2. 最大 / 最小
    if matches!(
        trimmed,
        "音量最大"
            | "声音最大"
            | "音量调到最大"
            | "声音调到最大"
            | "最大音量"
            | "最大声音"
            | "音量开到最大"
            | "声音开到最大"
    ) {
        return Some(VolumeAction::Set(100));
    }
    if matches!(
        trimmed,
        "音量最小"
            | "声音最小"
            | "音量调到最小"
            | "声音调到最小"
            | "最小音量"
            | "最小声音"
    ) {
        return Some(VolumeAction::Set(10));
    }

    // 3. 精确数值设置（支持阿拉伯数字、百分比与中文数字）
    const SET_PREFIXES: &[&str] = &[
        "音量设置到",
        "声音设置到",
        "音量设置为",
        "声音设置为",
        "音量调到",
        "声音调到",
        "音量调为",
        "声音调为",
        "音量调成",
        "声音调成",
        "音量设为",
        "声音设为",
        "音量改到",
        "声音改到",
        "音量改为",
        "声音改为",
        "音量开到",
        "声音开到",
        "调音量到",
        "调声音到",
        "音量到",
        "声音到",
    ];
    for prefix in SET_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if let Some(level) = parse_volume_number(rest) {
                return Some(VolumeAction::Set(level));
            }
        }
    }

    // 4. 调大音量
    if matches!(
        trimmed,
        "声音大一点"
            | "声音大点"
            | "声音大一些"
            | "大声一点"
            | "大声点"
            | "大声一些"
            | "调大音量"
            | "调大声音"
            | "音量调大"
            | "声音调大"
            | "音量大一点"
            | "音量大点"
            | "音量大一些"
            | "音量加"
            | "加音量"
            | "加大音量"
            | "提高音量"
            | "音量提高"
            | "大点声"
            | "声音放大"
            | "放大声音"
            | "音量放大"
            | "放大音量"
            | "音量增加"
            | "增加音量"
            | "声音调高"
            | "调高音量"
            | "音量调高"
            | "声音高一点"
            | "音量高一点"
    ) {
        return Some(VolumeAction::Up);
    }

    // 5. 调小音量
    if matches!(
        trimmed,
        "声音小一点"
            | "声音小点"
            | "声音小一些"
            | "小声一点"
            | "小声点"
            | "小声一些"
            | "调小音量"
            | "调小声音"
            | "音量调小"
            | "声音调小"
            | "音量小一点"
            | "音量小点"
            | "音量小一些"
            | "音量减"
            | "减音量"
            | "减小音量"
            | "降低音量"
            | "音量降低"
            | "小点声"
            | "声音放小"
            | "放小声音"
            | "音量放小"
            | "放小音量"
            | "音量减少"
            | "减少音量"
            | "声音调低"
            | "调低音量"
            | "音量调低"
            | "声音低一点"
            | "音量低一点"
    ) {
        return Some(VolumeAction::Down);
    }

    None
}

fn strip_polite_prefix(mut s: &str) -> &str {
    loop {
        if let Some(rest) = s.strip_prefix("请帮我把") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("帮我把") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("请把") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("帮我") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix('请') {
            s = rest;
        } else if let Some(rest) = s.strip_prefix('把') {
            s = rest;
        } else {
            return s;
        }
    }
}

fn parse_volume_number(raw: &str) -> Option<u8> {
    let s = raw
        .trim()
        .strip_prefix("百分之")
        .unwrap_or(raw.trim())
        .trim_end_matches('%')
        .trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<u16>().ok().filter(|&v| v <= 100).map(|v| v as u8);
    }
    parse_chinese_number_0_to_100(s)
}

fn parse_chinese_number_0_to_100(s: &str) -> Option<u8> {
    match s {
        "零" | "〇" => return Some(0),
        "一百" | "百" => return Some(100),
        _ => {}
    }
    if let Some((tens_part, ones_part)) = s.split_once('十') {
        let tens = if tens_part.is_empty() {
            1
        } else {
            cn_digit(tens_part)?
        };
        let ones = if ones_part.is_empty() {
            0
        } else {
            cn_digit(ones_part)?
        };
        return Some(tens * 10 + ones);
    }
    cn_digit(s)
}

fn cn_digit(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match ch {
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

/// 统一本地意图路由：按优先级判定唤醒词/空噪音/音量/音乐控制/退出/普通对话。
#[allow(dead_code)]
pub fn route_local_intent(
    raw_text: &str,
    has_music: bool,
    custom_exit_keywords: &[String],
) -> LocalIntent {
    let raw = raw_text.trim();
    if !raw.is_empty() && is_wake_word_only(raw) {
        return LocalIntent::WakeWordOnly;
    }

    let cleaned = strip_wake_prefix(raw);
    if cleaned.is_empty() || is_noise_or_filler(&cleaned) {
        return LocalIntent::EmptyOrNoise;
    }

    if let Some(action) = classify_volume_command(&cleaned) {
        return LocalIntent::Volume(action);
    }

    if has_music {
        if let Some(action) = classify_music_voice_control(&cleaned) {
            return LocalIntent::Music(action);
        }
    }

    if let Some(action) = classify_exit_command(&cleaned, custom_exit_keywords) {
        return LocalIntent::Exit(action);
    }

    LocalIntent::Chat(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_volume_command_matches_all_supported_variants() {
        // Up
        for phrase in [
            "声音大一点",
            "大声一点",
            "调大音量",
            "音量大一点",
            "音量加",
            "大点声",
            "声音放大",
            "请把声音大一点吧",
        ] {
            assert_eq!(
                classify_volume_command(phrase),
                Some(VolumeAction::Up),
                "expected Up for {phrase}"
            );
        }

        // Down
        for phrase in [
            "声音小一点",
            "小声一点",
            "调小音量",
            "音量小一点",
            "音量减",
            "小点声",
            "声音调小",
            "帮我调小音量啊",
        ] {
            assert_eq!(
                classify_volume_command(phrase),
                Some(VolumeAction::Down),
                "expected Down for {phrase}"
            );
        }

        // Mute
        for phrase in ["静音", "关闭声音", "声音调到零", "静音吧"] {
            assert_eq!(
                classify_volume_command(phrase),
                Some(VolumeAction::Mute),
                "expected Mute for {phrase}"
            );
        }

        // Set(n)
        assert_eq!(classify_volume_command("音量调到50"), Some(VolumeAction::Set(50)));
        assert_eq!(classify_volume_command("声音调到80%"), Some(VolumeAction::Set(80)));
        assert_eq!(classify_volume_command("音量设为30"), Some(VolumeAction::Set(30)));
        assert_eq!(classify_volume_command("音量最大"), Some(VolumeAction::Set(100)));
        assert_eq!(classify_volume_command("音量最小"), Some(VolumeAction::Set(10)));
        assert_eq!(classify_volume_command("音量调到五十"), Some(VolumeAction::Set(50)));
        assert_eq!(classify_volume_command("音量调到三十"), Some(VolumeAction::Set(30)));
        assert_eq!(classify_volume_command("声音调到八十"), Some(VolumeAction::Set(80)));
        assert_eq!(classify_volume_command("音量调到一百"), Some(VolumeAction::Set(100)));
        assert_eq!(classify_volume_command("音量调到百分之六十五"), Some(VolumeAction::Set(65)));
    }

    #[test]
    fn classify_volume_command_rejects_general_questions_and_non_commands() {
        for phrase in [
            "音量是什么原理",
            "声音为什么这么大",
            "声音在空气中传播速度是多少",
            "今天外面风声大一点也没关系",
            "音量调到150",
            "",
        ] {
            assert_eq!(
                classify_volume_command(phrase),
                None,
                "must NOT match {phrase}"
            );
        }
    }

    #[test]
    fn route_local_intent_covers_all_six_variants_in_priority_order() {
        // 1. WakeWordOnly
        assert_eq!(
            route_local_intent("小爱同学！", false, &[]),
            LocalIntent::WakeWordOnly
        );

        // 2. EmptyOrNoise
        assert_eq!(route_local_intent("   ", false, &[]), LocalIntent::EmptyOrNoise);
        assert_eq!(
            route_local_intent("小爱同学，嗯。", false, &[]),
            LocalIntent::EmptyOrNoise
        );

        // 3. Volume
        assert_eq!(
            route_local_intent("小爱同学，音量调到八十", false, &[]),
            LocalIntent::Volume(VolumeAction::Set(80))
        );

        // 4. Music (only when has_music=true)
        assert_eq!(
            route_local_intent("下一首", true, &[]),
            LocalIntent::Music(MusicVoiceControl::Next)
        );
        assert_eq!(
            route_local_intent("下一首", false, &[]),
            LocalIntent::Chat("下一首".to_string())
        );

        // 5. Exit (including custom keywords)
        assert_eq!(
            route_local_intent("闭嘴", false, &[]),
            LocalIntent::Exit(ExitAction::Silent)
        );
        assert_eq!(
            route_local_intent("再见", false, &[]),
            LocalIntent::Exit(ExitAction::Farewell)
        );
        assert_eq!(
            route_local_intent("退朝", false, &["退朝".to_string()]),
            LocalIntent::Exit(ExitAction::Farewell)
        );

        // 6. Chat (with wake prefix stripped)
        assert_eq!(
            route_local_intent("小爱同学，音量是什么原理", false, &[]),
            LocalIntent::Chat("音量是什么原理".to_string())
        );
    }
}
