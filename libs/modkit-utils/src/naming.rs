/// Convert a kebab-case or mixed-case module name to a `snake_case` prefix
/// suitable for metrics, logging scopes, and similar identifiers.
///
/// Rules:
/// - Hyphens and spaces become underscores.
/// - Runs of uppercase letters are lowered, with word boundaries inserted
///   before the last letter of a run followed by a lowercase letter
///   (e.g., `HTTPSProxy` → `https_proxy`).
/// - Leading/trailing underscores and consecutive underscores are collapsed.
///
/// # Examples
///
/// ```
/// use modkit_utils::naming::to_snake_case;
///
/// assert_eq!(to_snake_case("mini-chat"), "mini_chat");
/// assert_eq!(to_snake_case("UserSettings"), "user_settings");
/// assert_eq!(to_snake_case("LLMGateway"), "llm_gateway");
/// assert_eq!(to_snake_case("my_module"), "my_module");
/// assert_eq!(to_snake_case(""), "");
/// ```
#[must_use]
pub fn to_snake_case(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 4);
    let mut prev_was_upper = false;
    let mut prev_was_sep = false;
    let chars: Vec<char> = input.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        if ch == '-' || ch == ' ' {
            if !result.is_empty() {
                prev_was_sep = true;
            }
            prev_was_upper = false;
            continue;
        }

        if ch == '_' {
            if !result.is_empty() {
                prev_was_sep = true;
            }
            prev_was_upper = false;
            continue;
        }

        if ch.is_uppercase() {
            // Insert underscore before an uppercase letter when:
            // - it follows a lowercase/digit letter, OR
            // - it is the start of a new word in a run of uppercase
            //   (e.g., the 'P' in "HTTPProxy")
            let next_is_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            let needs_sep =
                !result.is_empty() && !prev_was_sep && (!prev_was_upper || next_is_lower);
            if needs_sep || (prev_was_sep && !result.is_empty()) {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            prev_was_upper = true;
            prev_was_sep = false;
        } else {
            if prev_was_sep && !result.is_empty() {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            prev_was_upper = false;
            prev_was_sep = false;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_case() {
        assert_eq!(to_snake_case("mini-chat"), "mini_chat");
        assert_eq!(to_snake_case("api-gateway"), "api_gateway");
        assert_eq!(to_snake_case("file-parser"), "file_parser");
    }

    #[test]
    fn already_snake_case() {
        assert_eq!(to_snake_case("my_module"), "my_module");
        assert_eq!(to_snake_case("mini_chat"), "mini_chat");
    }

    #[test]
    fn pascal_case() {
        assert_eq!(to_snake_case("UserSettings"), "user_settings");
        assert_eq!(to_snake_case("MiniChat"), "mini_chat");
    }

    #[test]
    fn upper_acronyms() {
        assert_eq!(to_snake_case("LLMGateway"), "llm_gateway");
        assert_eq!(to_snake_case("HTTPSProxy"), "https_proxy");
        assert_eq!(to_snake_case("OAGWModule"), "oagw_module");
    }

    #[test]
    fn empty_and_single() {
        assert_eq!(to_snake_case(""), "");
        assert_eq!(to_snake_case("a"), "a");
        assert_eq!(to_snake_case("A"), "a");
    }

    #[test]
    fn mixed_separators() {
        assert_eq!(to_snake_case("my-Cool_Module"), "my_cool_module");
        assert_eq!(to_snake_case("--leading--"), "leading");
    }
}
