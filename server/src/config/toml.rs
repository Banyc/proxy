pub fn human_toml_error(file_path: &str, src: &str, e: toml::de::Error) -> String {
    let Some(span) = e.span() else {
        return format!("{e}");
    };
    let affected = src.get(span.clone()).unwrap_or_default();
    let before = src.get(..span.start).unwrap_or_default();
    let line = 1 + before.matches('\n').count();
    let col = 1 + before.chars().rev().take_while(|&c| c != '\n').count();
    let msg = e.message();
    format!(
        "{msg}
File `{file_path}`
Line {line}, Column {col}
Affected: #'{affected}'#"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Config {
        #[expect(dead_code)]
        pub listen_addr: String,
    }

    fn error_for(src: &str) -> String {
        let e = toml::from_str::<Config>(src).unwrap_err();
        human_toml_error("config.toml", src, e)
    }

    #[test]
    fn a_non_ascii_character_does_not_move_the_reported_location() {
        let msg = error_for("# 配置文件\nlisten_addr = 1\n");
        assert!(msg.contains("Line 2,"), "{msg}");
        assert!(msg.contains("Affected: #'1'#"), "{msg}");
    }
    #[test]
    fn a_column_is_one_based_on_every_line() {
        let msg = error_for("listen_addr = 1\n");
        assert!(msg.contains("Line 1, Column 15"), "{msg}");
        let msg = error_for("\nlisten_addr = 1\n");
        assert!(msg.contains("Line 2, Column 15"), "{msg}");
    }
}
