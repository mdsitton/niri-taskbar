//! The key bindings from the Niri config, so the window menu can show the shortcut for anything
//! that has one.
//!
//! Niri doesn't hand its bindings out over IPC, so this reads its config file. Only as much of KDL
//! is understood as it takes to find the `binds` section and read the action out of each bind:
//! nodes, children, strings, comments, and includes.

use std::path::{Path, PathBuf};

/// A single key binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// The key combination as Niri writes it, like `Mod+Shift+F`.
    pub keys: String,
    pub action: String,
    pub args: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Binds(Vec<Bind>);

impl Binds {
    /// Reads the bindings from the Niri config, or returns none if it can't be found or read.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };

        let mut binds = Vec::new();
        read_file(&path, &mut binds, 0);
        Self(binds)
    }

    /// Returns the keys for the first bind that runs the given action with the given arguments.
    pub fn find(&self, action: &str, args: &[&str]) -> Option<&str> {
        self.0
            .iter()
            .find(|bind| bind.action == action && bind.args.iter().eq(args.iter()))
            .map(|bind| bind.keys.as_str())
    }
}

/// Where Niri reads its config from.
fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NIRI_CONFIG") {
        return Some(PathBuf::from(path));
    }

    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    let user = config.join("niri/config.kdl");
    if user.exists() {
        return Some(user);
    }

    let system = PathBuf::from("/etc/niri/config.kdl");
    system.exists().then_some(system)
}

fn read_file(path: &Path, binds: &mut Vec<Bind>, depth: usize) {
    // Includes can include each other; this is plenty deep for anything sensible.
    if depth > 8 {
        return;
    }
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };

    for node in parse(&source) {
        match node.name.as_str() {
            "binds" => binds.extend(node.children.iter().filter_map(bind)),
            "include" => {
                if let Some(include) = node.args.first() {
                    let include = Path::new(include);
                    let include = match path.parent() {
                        Some(dir) if include.is_relative() => dir.join(include),
                        _ => include.to_path_buf(),
                    };
                    read_file(&include, binds, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// Turns a node from the `binds` section into a bind: the node's name is the key combination, and
/// its first child is the action.
fn bind(node: &Node) -> Option<Bind> {
    let action = node.children.first()?;
    Some(Bind {
        keys: node.name.clone(),
        action: action.name.clone(),
        args: action.args.clone(),
    })
}

#[derive(Debug, Default)]
struct Node {
    name: String,
    /// Positional arguments. Properties (`key=value`) aren't needed, so they're dropped.
    args: Vec<String>,
    children: Vec<Node>,
}

fn parse(source: &str) -> Vec<Node> {
    let mut parser = Parser {
        chars: source.chars().collect(),
        pos: 0,
    };
    parser.nodes(false)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Reads nodes until the end of the input, or the closing brace of a children block.
    fn nodes(&mut self, in_block: bool) -> Vec<Node> {
        let mut nodes = Vec::new();
        loop {
            self.skip_space(true);
            match self.peek() {
                None => break,
                Some('}') if in_block => {
                    self.pos += 1;
                    break;
                }
                Some(';' | '}') => self.pos += 1,
                Some('/') if self.peek_at(1) == Some('-') => {
                    // A slashdash comments out the whole node after it.
                    self.pos += 2;
                    self.skip_space(true);
                    self.node();
                }
                Some(_) => {
                    let start = self.pos;
                    match self.node() {
                        Some(node) => nodes.push(node),
                        // Something we don't understand; skip it rather than looping forever.
                        None if self.pos == start => self.pos += 1,
                        None => {}
                    }
                }
            }
        }
        nodes
    }

    /// Reads one node: a name, then arguments and properties, then maybe a children block.
    fn node(&mut self) -> Option<Node> {
        self.skip_annotation();
        let name = self.value()?;
        let mut node = Node {
            name,
            ..Default::default()
        };

        loop {
            self.skip_space(false);
            match self.peek() {
                None | Some('\n' | '\r' | ';') => break,
                Some('}') => break,
                Some('{') => {
                    self.pos += 1;
                    node.children = self.nodes(true);
                    break;
                }
                Some('/') if self.peek_at(1) == Some('-') => {
                    // Slashdash on an argument or a children block drops just that.
                    self.pos += 2;
                    self.skip_space(false);
                    if self.peek() == Some('{') {
                        self.pos += 1;
                        self.nodes(true);
                    } else {
                        self.skip_annotation();
                        self.value();
                        self.property_value();
                    }
                }
                Some(_) => {
                    self.skip_annotation();
                    let Some(value) = self.value() else {
                        // Something we don't understand; skip it rather than looping forever.
                        self.pos += 1;
                        continue;
                    };
                    if !self.property_value() {
                        node.args.push(value);
                    }
                }
            }
        }

        Some(node)
    }

    /// If a property's `=` follows, reads and discards its value, returning true.
    fn property_value(&mut self) -> bool {
        if self.peek() != Some('=') {
            return false;
        }
        self.pos += 1;
        self.skip_annotation();
        self.value();
        true
    }

    /// Skips a type annotation, like `(u8)`.
    fn skip_annotation(&mut self) {
        if self.peek() == Some('(') {
            while let Some(c) = self.peek() {
                self.pos += 1;
                if c == ')' {
                    break;
                }
            }
        }
    }

    /// Reads a string, raw string or bare value.
    fn value(&mut self) -> Option<String> {
        match self.peek()? {
            '"' => Some(self.string()),
            'r' if matches!(self.peek_at(1), Some('"' | '#')) => Some(self.raw_string()),
            '#' if matches!(self.peek_at(1), Some('"' | '#')) => Some(self.raw_string()),
            _ => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_whitespace() || "{}();=\"".contains(c) {
                        break;
                    }
                    if c == '/' && matches!(self.peek_at(1), Some('/' | '*')) {
                        break;
                    }
                    self.pos += 1;
                }
                (self.pos > start).then(|| self.chars[start..self.pos].iter().collect())
            }
        }
    }

    fn string(&mut self) -> String {
        self.pos += 1;
        let mut value = String::new();
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                '"' => break,
                '\\' => {
                    let escaped = self.peek().unwrap_or('\\');
                    self.pos += 1;
                    value.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                }
                c => value.push(c),
            }
        }
        value
    }

    /// Reads a raw string, KDL 1's `r#"..."#` or KDL 2's `#"..."#`.
    fn raw_string(&mut self) -> String {
        if self.peek() == Some('r') {
            self.pos += 1;
        }
        let mut hashes = 0;
        while self.peek() == Some('#') {
            hashes += 1;
            self.pos += 1;
        }
        self.pos += 1;

        let start = self.pos;
        while self.pos < self.chars.len() {
            if self.chars[self.pos] == '"' && (1..=hashes).all(|i| self.peek_at(i) == Some('#')) {
                let value = self.chars[start..self.pos].iter().collect();
                self.pos += 1 + hashes;
                return value;
            }
            self.pos += 1;
        }
        self.chars[start..].iter().collect()
    }

    /// Skips whitespace and comments, and newlines too if `newlines` is set. A backslash
    /// continues a node onto the next line.
    fn skip_space(&mut self, newlines: bool) {
        while let Some(c) = self.peek() {
            match c {
                '\n' | '\r' if !newlines => break,
                '\\' => {
                    self.pos += 1;
                    self.skip_space(false);
                    if matches!(self.peek(), Some('\r')) {
                        self.pos += 1;
                    }
                    if matches!(self.peek(), Some('\n')) {
                        self.pos += 1;
                    }
                }
                '/' if self.peek_at(1) == Some('/') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.pos += 1;
                    }
                }
                '/' if self.peek_at(1) == Some('*') => {
                    // Block comments nest.
                    self.pos += 2;
                    let mut depth = 1;
                    while depth > 0 && self.pos < self.chars.len() {
                        if self.peek() == Some('/') && self.peek_at(1) == Some('*') {
                            depth += 1;
                            self.pos += 2;
                        } else if self.peek() == Some('*') && self.peek_at(1) == Some('/') {
                            depth -= 1;
                            self.pos += 2;
                        } else {
                            self.pos += 1;
                        }
                    }
                }
                c if c.is_whitespace() => self.pos += 1,
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binds(source: &str) -> Vec<Bind> {
        parse(source)
            .into_iter()
            .filter(|node| node.name == "binds")
            .flat_map(|node| node.children.iter().filter_map(bind).collect::<Vec<_>>())
            .collect()
    }

    #[test]
    fn reads_binds() {
        let source = r#"
            input { keyboard { xkb { layout "us"; } } }
            // A comment { with braces }
            binds {
                Mod+Q repeat=false { close-window; }
                Mod+T hotkey-overlay-title="Open a Terminal: alacritty" { spawn "alacritty"; }
                /-Mod+X { quit; }
                /* Mod+Y { quit; } */
                Mod+Shift+1 { move-window-to-workspace 1; }
                Mod+V {
                    toggle-window-floating
                }
            }
        "#;

        let found = binds(source);
        let summary: Vec<_> = found
            .iter()
            .map(|b| (b.keys.as_str(), b.action.as_str(), b.args.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Mod+Q", "close-window", vec![]),
                ("Mod+T", "spawn", vec!["alacritty".to_string()]),
                (
                    "Mod+Shift+1",
                    "move-window-to-workspace",
                    vec!["1".to_string()]
                ),
                ("Mod+V", "toggle-window-floating", vec![]),
            ]
        );
    }

    #[test]
    fn finds_by_action_and_args() {
        let binds = Binds(binds(
            r#"binds {
                Mod+Shift+1 { move-window-to-workspace 1; }
                Mod+Shift+2 { move-window-to-workspace 2; }
                Mod+F { maximize-column; }
            }"#,
        ));
        assert_eq!(binds.find("maximize-column", &[]), Some("Mod+F"));
        assert_eq!(
            binds.find("move-window-to-workspace", &["2"]),
            Some("Mod+Shift+2")
        );
        assert_eq!(binds.find("move-window-to-workspace", &["3"]), None);
    }

    #[test]
    fn raw_strings_and_escapes() {
        let nodes = parse(r##"a r#"x "quoted" y"# "b\"c"; b #"kdl2"#"##);
        assert_eq!(nodes[0].args, vec!["x \"quoted\" y", "b\"c"]);
        assert_eq!(nodes[1].args, vec!["kdl2"]);
    }
}
