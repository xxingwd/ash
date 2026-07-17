use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn truncate_end(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    let mut output = String::new();
    let mut used = 0;
    let available = width.saturating_sub(1);
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > available {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

pub(crate) fn truncate_start(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    let mut suffix = String::new();
    let mut used = 0;
    let available = width.saturating_sub(1);
    for character in value.chars().rev() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > available {
            break;
        }
        suffix.insert(0, character);
        used += character_width;
    }
    format!("…{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_both_ends_on_unicode_width_boundaries() {
        assert_eq!(truncate_end("中文说明", 5), "中文…");
        assert_eq!(truncate_start("~/workspace/ash", 8), "…ace/ash");
    }
}
