//! User-facing messages follow the application preference, then the OS locale.
pub fn text<'a>(english: &'a str, chinese: &'a str) -> &'a str {
    static CHINESE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let chinese_selected = CHINESE.get_or_init(|| {
        let language = crate::runtime::resolve_default_data_dir()
            .ok()
            .and_then(|root| std::fs::read(root.join("config.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["language"].as_str().map(str::to_owned));
        select(language.as_deref(), &system_language(), "en", "zh") == "zh"
    });
    if *chinese_selected {
        chinese
    } else {
        english
    }
}
fn select<'a>(setting: Option<&str>, system: &str, english: &'a str, chinese: &'a str) -> &'a str {
    let language = setting
        .filter(|s| !s.is_empty() && !matches!(*s, "system" | "auto"))
        .unwrap_or(system);
    if language.to_ascii_lowercase().starts_with("zh") {
        chinese
    } else {
        english
    }
}
#[cfg(windows)]
fn system_language() -> String {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, count: i32) -> i32;
    }
    let mut buffer = [0u16; 85];
    let count = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..count.saturating_sub(1).max(0) as usize])
}
#[cfg(not(windows))]
fn system_language() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "en".into())
}
#[cfg(test)]
mod tests {
    #[test]
    fn preference_overrides_system_and_auto_follows_it() {
        assert_eq!(super::select(Some("en"), "zh-CN", "en", "zh"), "en");
        assert_eq!(super::select(Some("zh-CN"), "en-US", "en", "zh"), "zh");
        assert_eq!(super::select(Some("system"), "zh-CN", "en", "zh"), "zh");
        assert_eq!(super::select(None, "en-US", "en", "zh"), "en");
    }
}
