pub fn human_toml_error(file_path: &str, src: &str, e: toml::de::Error) -> String {
    let Some(span) = e.span() else {
        return format!("{e}");
    };
    let affected = src
        .chars()
        .skip(span.start)
        .take(span.end - span.start)
        .collect::<String>();
    let (line, col) = {
        let mut line = 1;
        let mut col = 1;

        for (i, char) in src.chars().enumerate() {
            if i == span.start {
                break;
            }
            if char == '\n' {
                line += 1;
                col = 1;
            }
            col += 1;
        }

        (line, col)
    };
    let msg = e.message();
    let e = format!(
        "{msg}
File `{file_path}`
Line {line}, Column {col}
Affected: #'{affected}'#"
    );
    e
}
