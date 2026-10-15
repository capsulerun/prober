//! Single-file components (`.vue`, `.svelte`): their script blocks are parsed in place of the file, with the template and styles blanked so every position keeps its line.

use std::borrow::Cow;

use oxc_span::SourceType;

pub(crate) const COMPONENT_EXTENSIONS: [&str; 2] = ["vue", "svelte"];

const SCRIPT_OPEN: &str = "<script";

const SCRIPT_CLOSE: &str = "</script>";

pub(crate) fn is_component(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| COMPONENT_EXTENSIONS.contains(&extension))
}

pub(crate) fn parse_input<'s>(path: &str, source_text: &'s str) -> (SourceType, Cow<'s, str>) {
    if !is_component(path) {
        return (
            SourceType::from_path(path).unwrap_or_default(),
            Cow::Borrowed(source_text),
        );
    }

    let mut script = String::with_capacity(source_text.len());
    let mut source_type = SourceType::mjs();
    let mut rest = source_text;

    while let Some(open) = rest.find(SCRIPT_OPEN) {
        let Some(tag_end) = rest[open..].find('>').map(|end| open + end + 1) else {
            break;
        };
        let content_end = rest[tag_end..]
            .find(SCRIPT_CLOSE)
            .map_or(rest.len(), |end| tag_end + end);

        source_type = script_type(&rest[open..tag_end]).unwrap_or(source_type);
        script.push_str(&blanked(&rest[..tag_end]));
        script.push_str(&rest[tag_end..content_end]);
        rest = &rest[content_end..];
    }

    script.push_str(&blanked(rest));

    (source_type, Cow::Owned(script))
}

fn script_type(opening_tag: &str) -> Option<SourceType> {
    let lang = opening_tag
        .split_once("lang=")?
        .1
        .trim_start_matches(['"', '\''])
        .split(['"', '\'', ' ', '>'])
        .next()?;

    match lang {
        "ts" => Some(SourceType::ts()),
        "tsx" => Some(SourceType::tsx()),
        "jsx" => Some(SourceType::jsx()),
        _ => None,
    }
}

fn blanked(text: &str) -> String {
    let mut blank = String::with_capacity(text.len());

    for character in text.chars() {
        if character == '\n' || character == '\r' {
            blank.push(character);
        } else {
            blank.extend(std::iter::repeat_n(' ', character.len_utf8()));
        }
    }

    blank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_blocks_keep_their_lines_and_the_rest_is_blanked() {
        let component = "<template>\n  <N8nChatInput />\n</template>\n<script setup lang=\"ts\">\nimport N8nChatInput from '../N8nChatInput';\n</script>\n<style>.a { color: red; }</style>\n";
        let (source_type, script) = parse_input("src/Chat.vue", component);

        assert!(source_type.is_typescript());
        assert_eq!(script.len(), component.len());
        assert_eq!(
            script.lines().nth(4),
            Some("import N8nChatInput from '../N8nChatInput';")
        );
        assert!(!script.contains("template") && !script.contains("color"));
    }

    #[test]
    fn other_files_are_parsed_as_they_are() {
        let (source_type, script) = parse_input("src/chat.ts", "export const a = 1;");

        assert!(source_type.is_typescript());
        assert_eq!(script, "export const a = 1;");
    }
}
