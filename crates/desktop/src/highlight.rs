//! Lexical syntax highlighting for code blocks.
//!
//! One tokenizer covers every language: comments, strings, numbers,
//! keywords, function calls and type-like names. Per-language tables only
//! choose comment syntax and keywords. It is not a parser, so it can be fooled
//! by exotic syntax, but it costs microseconds and handles half-streamed code.

use std::ops::Range;

/// What a highlighted range is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// Reserved word.
    Keyword,
    /// String or character literal.
    String,
    /// Numeric literal (also booleans and null-like constants).
    Number,
    /// Comment.
    Comment,
    /// Name followed by a call.
    Function,
    /// Capitalised name, tag or attribute.
    Type,
}

/// Languages whose quirks the tokenizer special-cases.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Rust,
    Sql,
    Dashed,
    Other,
}

/// Comment syntax and keywords of one language family.
struct Lang {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    keywords: &'static [&'static str],
    constants: &'static [&'static str],
    /// Capitalised identifiers are types (false for SQL, shells…).
    types: bool,
    /// Special cases: Rust lifetimes and macros, case-insensitive SQL, and
    /// languages whose names contain `-` (CSS, shells).
    kind: Kind,
    /// Markup: `<tag` names are highlighted.
    markup: bool,
}

const C_LIKE: &[&str] = &[
    "auto", "break", "case", "catch", "class", "const", "continue", "default", "delete", "do", "else", "enum", "explicit",
    "export", "extern", "for", "friend", "goto", "if", "inline", "namespace", "new", "operator", "private", "protected",
    "public", "return", "sizeof", "static", "struct", "switch", "template", "this", "throw", "try", "typedef", "typename",
    "union", "using", "virtual", "void", "volatile", "while", "int", "char", "float", "double", "long", "short", "unsigned",
    "signed", "bool", "include", "define", "ifdef", "ifndef", "endif", "pragma", "override", "final", "abstract", "extends",
    "implements", "import", "package", "interface", "instanceof", "super", "synchronized", "throws", "var", "val", "fun",
    "when", "is", "in", "out", "object", "companion", "data", "sealed", "internal", "open", "lateinit", "func", "let",
    "guard", "defer", "protocol", "extension", "self", "Self", "async", "await", "yield", "foreach", "readonly", "ref",
    "params", "base", "get", "set", "partial", "record", "where", "select", "from", "string", "object", "dynamic", "lock",
    "checked", "unchecked", "fixed", "stackalloc", "event", "delegate", "operator", "implicit", "sealed", "noexcept",
    "constexpr", "nullptr_t", "mutable", "static_cast", "dynamic_cast", "reinterpret_cast", "const_cast", "co_await",
];
const C_CONSTANTS: &[&str] = &["true", "false", "null", "nullptr", "NULL", "nil", "undefined", "None", "YES", "NO"];

const RUST: Lang = Lang {
    line: &["//"],
    block: Some(("/*", "*/")),
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "fn", "for", "if",
        "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
        "struct", "super", "trait", "type", "unsafe", "use", "where", "while", "union", "macro_rules", "yield",
    ],
    constants: &["true", "false", "None", "Some", "Ok", "Err"],
    types: true,
    kind: Kind::Rust,
    markup: false,
};
const PYTHON: Lang = Lang {
    line: &["#"],
    block: None,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
        "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise",
        "return", "try", "while", "with", "yield", "match", "case", "self", "print",
    ],
    constants: &["True", "False", "None"],
    types: true,
    kind: Kind::Other,
    markup: false,
};
const JS: Lang = Lang {
    line: &["//"],
    block: Some(("/*", "*/")),
    keywords: &[
        "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do",
        "else", "export", "extends", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "let", "new",
        "of", "return", "static", "super", "switch", "this", "throw", "try", "typeof", "var", "void", "while", "with",
        "yield", "interface", "type", "enum", "implements", "private", "public", "protected", "readonly", "as", "declare",
        "namespace", "abstract", "keyof", "satisfies", "get", "set",
    ],
    constants: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    types: true,
    kind: Kind::Other,
    markup: false,
};
const GO: Lang = Lang {
    line: &["//"],
    block: Some(("/*", "*/")),
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto",
        "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var",
    ],
    constants: &["true", "false", "nil", "iota"],
    types: true,
    kind: Kind::Other,
    markup: false,
};
const C_FAMILY: Lang = Lang { line: &["//"], block: Some(("/*", "*/")), keywords: C_LIKE, constants: C_CONSTANTS, types: true, kind: Kind::Other, markup: false };
const SHELL: Lang = Lang {
    line: &["#"],
    block: None,
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function",
        "return", "exit", "export", "local", "readonly", "source", "echo", "cd", "set", "unset", "shift", "sudo", "param",
        "foreach", "begin", "process", "end", "try", "catch", "finally", "throw",
    ],
    constants: &["true", "false", "$true", "$false", "$null"],
    types: false,
    kind: Kind::Dashed,
    markup: false,
};
const SQL: Lang = Lang {
    line: &["--"],
    block: Some(("/*", "*/")),
    keywords: &[
        "select", "from", "where", "and", "or", "not", "insert", "into", "values", "update", "set", "delete", "create",
        "table", "drop", "alter", "add", "index", "primary", "key", "foreign", "references", "join", "left", "right",
        "inner", "outer", "on", "as", "group", "by", "order", "having", "limit", "offset", "union", "all", "distinct",
        "case", "when", "then", "else", "end", "in", "is", "like", "between", "exists", "with", "returning", "default",
        "unique", "constraint", "view", "asc", "desc", "count", "sum", "avg", "min", "max", "integer", "text", "varchar",
        "boolean", "timestamp", "begin", "commit", "rollback",
    ],
    constants: &["null", "true", "false"],
    types: false,
    kind: Kind::Sql,
    markup: false,
};
const RUBY: Lang = Lang {
    line: &["#"],
    block: None,
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif", "end", "ensure", "for",
        "if", "in", "module", "next", "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "undef",
        "unless", "until", "when", "while", "yield", "require", "attr_accessor", "puts", "lambda", "proc",
    ],
    constants: &["true", "false", "nil"],
    types: true,
    kind: Kind::Other,
    markup: false,
};
const LUA: Lang = Lang {
    line: &["--"],
    block: Some(("--[[", "]]")),
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in", "local", "not", "or",
        "repeat", "return", "then", "until", "while",
    ],
    constants: &["true", "false", "nil"],
    types: false,
    kind: Kind::Other,
    markup: false,
};
const DATA: Lang = Lang { line: &["#"], block: None, keywords: &[], constants: &["true", "false", "null", "yes", "no", "on", "off"], types: false, kind: Kind::Other, markup: false };
const JSON: Lang = Lang { line: &["//"], block: None, keywords: &[], constants: &["true", "false", "null"], types: false, kind: Kind::Other, markup: false };
const MARKUP: Lang = Lang { line: &[], block: Some(("<!--", "-->")), keywords: &[], constants: &[], types: false, kind: Kind::Dashed, markup: true };
const CSS: Lang = Lang {
    line: &["//"],
    block: Some(("/*", "*/")),
    keywords: &["@media", "@import", "@keyframes", "@font-face", "!important", "@use", "@include", "@mixin", "@extend"],
    constants: &[],
    types: false,
    kind: Kind::Dashed,
    markup: false,
};
const HASKELL: Lang = Lang {
    line: &["--"],
    block: Some(("{-", "-}")),
    keywords: &["case", "class", "data", "deriving", "do", "else", "if", "import", "in", "instance", "let", "module", "of", "then", "type", "where", "newtype"],
    constants: &["True", "False", "Nothing", "Just"],
    types: true,
    kind: Kind::Other,
    markup: false,
};
const PLAIN: Lang = Lang { line: &[], block: None, keywords: &[], constants: &[], types: false, kind: Kind::Other, markup: false };

fn lang(name: &str) -> &'static Lang {
    match name {
        "rust" | "rs" => &RUST,
        "python" | "py" | "python3" | "gdscript" => &PYTHON,
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "mjs" | "cjs" | "node" => &JS,
        "go" | "golang" => &GO,
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "cxx" | "java" | "kotlin" | "kt" | "swift" | "csharp" | "cs" | "c#"
        | "objc" | "objective-c" | "dart" | "scala" | "zig" | "glsl" | "hlsl" | "wgsl" | "php" | "groovy" | "proto" => &C_FAMILY,
        "bash" | "sh" | "shell" | "zsh" | "fish" | "console" | "powershell" | "ps1" | "pwsh" | "bat" | "cmd" | "dockerfile"
        | "docker" | "makefile" | "make" => &SHELL,
        "sql" | "postgres" | "postgresql" | "mysql" | "sqlite" => &SQL,
        "ruby" | "rb" | "crystal" | "elixir" | "ex" | "exs" => &RUBY,
        "lua" => &LUA,
        "json" | "jsonc" | "json5" => &JSON,
        "yaml" | "yml" | "toml" | "ini" | "conf" | "env" | "properties" => &DATA,
        "html" | "xml" | "svg" | "vue" | "svelte" | "xaml" | "plist" => &MARKUP,
        "css" | "scss" | "sass" | "less" => &CSS,
        "haskell" | "hs" | "elm" => &HASKELL,
        _ => &PLAIN,
    }
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$' || c == '@'
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '-'
}

/// Highlighted ranges of `code` (sorted, non-overlapping; the rest is plain).
#[must_use]
pub fn highlight(language: &str, code: &str) -> Vec<(Range<usize>, Token)> {
    let lang = lang(&language.to_ascii_lowercase());
    let mut out = Vec::new();
    let mut i = 0;
    let rest = |i: usize| &code[i..];
    while i < code.len() {
        let tail = rest(i);
        let c = tail.chars().next().unwrap_or(' ');
        // Comments.
        if let Some(prefix) = lang.line.iter().find(|p| tail.starts_with(**p)) {
            // `#` only starts a comment at a word boundary (not `a#b`, `#include` is fine).
            let boundary = i == 0 || !code[..i].ends_with(|p: char| p.is_alphanumeric());
            if boundary || !prefix.starts_with('#') {
                let end = tail.find('\n').map_or(code.len(), |n| i + n);
                out.push((i..end, Token::Comment));
                i = end;
                continue;
            }
        }
        if let Some((open, close)) = lang.block
            && tail.starts_with(open)
        {
            let end = tail[open.len()..].find(close).map_or(code.len(), |n| i + open.len() + n + close.len());
            out.push((i..end, Token::Comment));
            i = end;
            continue;
        }
        // Strings: triple quotes, then single-line or backtick strings.
        if matches!(c, '"' | '\'' | '`') {
            let triple: String = std::iter::repeat_n(c, 3).collect();
            let (delim, multiline) = if tail.starts_with(&triple) { (triple.as_str(), true) } else { (&tail[..1], c == '`') };
            // In Rust, `'a` is a lifetime unless it closes like a char (`'a'`, `'\n'`).
            let lifetime = lang.kind == Kind::Rust
                && c == '\''
                && !tail[1..].starts_with('\\')
                && tail[1..].chars().nth(1).is_some_and(|n| n != '\'');
            if !lifetime {
                let mut j = i + delim.len();
                let mut end = code.len();
                while j < code.len() {
                    let t = &code[j..];
                    if let Some(escaped) = t.strip_prefix('\\') {
                        j += 1 + escaped.chars().next().map_or(0, char::len_utf8);
                        continue;
                    }
                    if t.starts_with(delim) {
                        end = j + delim.len();
                        break;
                    }
                    if t.starts_with('\n') && !multiline {
                        end = j;
                        break;
                    }
                    j += t.chars().next().map_or(1, char::len_utf8);
                }
                out.push((i..end, Token::String));
                i = end;
                continue;
            }
        }
        if lang.markup && c == '<' {
            let name_start = i + 1 + usize::from(tail[1..].starts_with('/'));
            let name_len = code[name_start..].find(|x: char| !(x.is_alphanumeric() || x == '-' || x == ':' || x == '_')).unwrap_or(code.len() - name_start);
            if name_len > 0 {
                out.push((name_start..name_start + name_len, Token::Keyword));
                i = name_start + name_len;
                continue;
            }
        }
        if c.is_ascii_digit() && !code[..i].ends_with(|p: char| ident_char(p)) {
            let len = tail
                .find(|x: char| !(x.is_ascii_alphanumeric() || x == '_' || x == '.'))
                .unwrap_or(tail.len());
            out.push((i..i + len, Token::Number));
            i += len;
            continue;
        }
        if ident_start(c) {
            let len = tail.find(|x: char| !ident_char(x)).unwrap_or(tail.len());
            // `-` belongs to CSS/shell names but not to `a-b` arithmetic.
            let len = if lang.kind == Kind::Dashed {
                len
            } else {
                tail[..len].find('-').unwrap_or(len)
            };
            let word = &tail[..len];
            let lower = word.to_ascii_lowercase();
            let next = tail[len..].trim_start_matches([' ', '\t']).chars().next();
            let sql = lang.kind == Kind::Sql;
            let token = if lang.keywords.contains(&word) || (sql && lang.keywords.contains(&lower.as_str())) {
                Some(Token::Keyword)
            } else if lang.constants.contains(&word) || (sql && lang.constants.contains(&lower.as_str())) {
                Some(Token::Number)
            } else if next == Some('(') || (next == Some('!') && lang.kind == Kind::Rust) {
                Some(Token::Function)
            } else if (lang.types && word.starts_with(|x: char| x.is_uppercase())) || (lang.markup && next == Some('=')) {
                Some(Token::Type)
            } else {
                None
            };
            if let Some(token) = token {
                out.push((i..i + len, token));
            }
            i += len.max(c.len_utf8());
            continue;
        }
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens<'a>(lang: &str, code: &'a str) -> Vec<(&'a str, Token)> {
        highlight(lang, code).into_iter().map(|(r, t)| (&code[r], t)).collect()
    }

    #[test]
    fn rust() {
        let got = tokens("rust", "fn main() { let x: Vec<u8> = vec![1, 0x2f]; // hi\n println!(\"a\\\"b\"); }");
        assert_eq!(
            got,
            [
                ("fn", Token::Keyword),
                ("main", Token::Function),
                ("let", Token::Keyword),
                ("Vec", Token::Type),
                ("vec", Token::Function),
                ("1", Token::Number),
                ("0x2f", Token::Number),
                ("// hi", Token::Comment),
                ("println", Token::Function),
                ("\"a\\\"b\"", Token::String),
            ]
        );
        // Lifetimes are not strings.
        assert!(tokens("rust", "fn f<'a>(x: &'a str)").iter().all(|(_, t)| *t != Token::String));
    }

    #[test]
    fn python_and_shell() {
        assert_eq!(tokens("py", "def f():\n    return None  # done"), [("def", Token::Keyword), ("f", Token::Function), ("return", Token::Keyword), ("None", Token::Number), ("# done", Token::Comment)]);
        assert_eq!(tokens("bash", "echo \"$HOME\" # c\nfoo#bar"), [("echo", Token::Keyword), ("\"$HOME\"", Token::String), ("# c", Token::Comment)]);
        assert_eq!(tokens("sql", "SELECT id FROM t"), [("SELECT", Token::Keyword), ("FROM", Token::Keyword)]);
    }

    #[test]
    fn unterminated_constructs_run_to_the_end() {
        assert_eq!(tokens("js", "/* open"), [("/* open", Token::Comment)]);
        assert_eq!(tokens("py", "x = \"\"\"doc\nmore"), [("\"\"\"doc\nmore", Token::String)]);
        assert!(tokens("unknown-lang", "anything { goes }").is_empty());
    }

    #[test]
    fn markup_tags() {
        assert_eq!(tokens("html", "<div class=\"a\"></div>"), [("div", Token::Keyword), ("class", Token::Type), ("\"a\"", Token::String), ("div", Token::Keyword)]);
    }
}
