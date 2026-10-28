//! Output paths and file names that are safe on both Linux and Windows.

/// Maximum length of one path component in UTF-8 bytes.
/// Stays below ext4's 255-byte and NTFS's 255-unit limits.
const MAX_COMPONENT_BYTES: usize = 200;

/// Extensions longer than this are treated as part of the stem when truncating.
const MAX_EXTENSION_BYTES: usize = 16;

const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Turns `name` into a single path component that is valid on Linux and Windows.
pub fn sanitize_component(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = replaced.trim().trim_end_matches(['.', ' ']);
    let mut name = if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    };
    let stem = name.split('.').next().unwrap_or_default();
    if WINDOWS_RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        name.insert(0, '_');
    }
    truncate_keep_extension(&name, MAX_COMPONENT_BYTES)
}

/// Directory name for an item: `<item_id>_<title>`.
pub fn item_dir_name(item_id: u64, title: &str) -> String {
    sanitize_component(&format!("{item_id}_{title}"))
}

/// File name used when `file_name` is already taken: `<stem> (<id>).<ext>`.
pub fn with_id_suffix(file_name: &str, id: u64) -> String {
    let (stem, ext) = split_extension(file_name);
    sanitize_component(&format!("{stem} ({id}){ext}"))
}

/// Splits `name` into stem and extension (the extension keeps its dot).
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= MAX_EXTENSION_BYTES => name.split_at(i),
        _ => (name, ""),
    }
}

fn truncate_keep_extension(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, ext) = split_extension(name);
    let mut end = max - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(['.', ' ']);
    format!("{stem}{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_windows_forbidden_characters() {
        assert_eq!(
            sanitize_component(r#"a<b>c:d"e/f\g|h?i*j"#),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_component("tab\there"), "tab_here");
    }

    #[test]
    fn strips_trailing_dots_and_spaces() {
        assert_eq!(sanitize_component("  name. . "), "name");
    }

    #[test]
    fn never_returns_empty_or_dot_names() {
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("."), "_");
        assert_eq!(sanitize_component(".."), "_");
    }

    #[test]
    fn prefixes_windows_reserved_names() {
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_component("CONSOLE"), "CONSOLE");
    }

    #[test]
    fn truncates_long_names_and_keeps_extension() {
        let name = format!("{}.zip", "あ".repeat(100)); // 300 bytes + ".zip"
        let sanitized = sanitize_component(&name);
        assert!(sanitized.len() <= MAX_COMPONENT_BYTES);
        assert!(sanitized.ends_with("あ.zip"));
    }

    #[test]
    fn builds_item_dir_name() {
        assert_eq!(item_dir_name(42, "Title: Vol.1"), "42_Title_ Vol.1");
    }

    #[test]
    fn adds_id_suffix_before_extension() {
        assert_eq!(with_id_suffix("data.zip", 7), "data (7).zip");
        assert_eq!(with_id_suffix("README", 7), "README (7)");
    }
}
