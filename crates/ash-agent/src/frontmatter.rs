pub(crate) fn parse(content: &str) -> Result<(&str, &str), &'static str> {
    let content = content.trim_start();
    let mut lines = content.split_inclusive('\n');
    let first_line = lines.next().unwrap_or_default();
    if first_line.trim_end_matches(['\r', '\n']) != "---" {
        return Err("file must start with ---");
    }
    let frontmatter_start = first_line.len();
    let mut offset = frontmatter_start;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((
                content[frontmatter_start..offset].trim(),
                content[offset + line.len()..].trim(),
            ));
        }
        offset += line.len();
    }
    Err("missing closing ---")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_crlf_and_a_closing_delimiter_at_end_of_file() {
        assert_eq!(parse("---\r\nname: test\r\n---"), Ok(("name: test", "")));
    }

    #[test]
    fn preserves_delimiters_in_the_body() {
        assert_eq!(
            parse("---\nname: test\n---\nbody\n---"),
            Ok(("name: test", "body\n---"))
        );
    }

    #[test]
    fn rejects_missing_delimiters() {
        assert!(parse("name: test").is_err());
        assert!(parse("---\nname: test").is_err());
    }
}
