use anyhow::Result;
use std::collections::HashMap;
use std::fmt;
use std::fs::read_to_string;
use std::path::Path;

pub struct Tokenizer {
    /// Stream of characters in the raw string
    pub raw: String,
    /// Current position in the raw string
    pub position: usize,
    /// List of tokens
    pub tokens: Vec<Token>,
}

#[derive(Debug, PartialEq)]
pub struct RawNamedBody {
    pub name: String,
    pub body: String,
}

#[derive(Debug)]
pub struct Token {
    pub token_type: TokenType,
    pub position: (usize, usize),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} at {}:{}",
            self.token_type,
            self.position.0 + 1,
            self.position.1 + 1
        )
    }
}

#[derive(Debug, PartialEq)]
pub enum TokenType {
    /// Literal tokens
    String(String),
    Int(i32),
    Boolean(bool),
    Reference(String),
    /// Keyword tokens
    Task,
    Description,
    Variables,
    Run,
    Include,
    Depends,
    Condition,
    Workdir,
    File(RawNamedBody),
    Of,
    Subtask,
    Invoked,
    /// Symbol tokens
    LeftParenthesis,
    RightParenthesis,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Semicolon,
    Colon,
    Comma,
    Bang,
    Pipe,
}

impl Tokenizer {
    pub fn from_file(path_str: &str) -> Result<Tokenizer> {
        let path = Path::new(path_str);
        if !path.exists() {
            return Err(anyhow::anyhow!("File {} not found", path_str));
        }
        let raw = read_to_string(path)?;
        Ok(Self {
            raw,
            position: 0,
            tokens: Vec::new(),
        })
    }

    fn is_reference_or_keyword(c: &char) -> bool {
        c.is_alphabetic()
            || c == &'_'
            || c == &'-'
            || c == &'/'
            || c == &'!'
            || c == &'.'
            || c.is_ascii_digit()
            || c == &'+'
    }

    pub fn run(&mut self) -> Result<()> {
        let mut macros = HashMap::new();
        while let Some(value) = self.peek_once() {
            match value {
                ' ' | '\n' | '\t' => self.skip(),
                '#' => {
                    self.read_until(|_, c| c == &'\n');
                }
                _ if value == '/' && self.peek_twice() == Some('*') => {
                    self.read_until(|prev, current| prev == Some('*') && current == &'/');
                    self.skip();
                }
                '(' => self.add_token_type_advance(TokenType::LeftParenthesis),
                ')' => self.add_token_type_advance(TokenType::RightParenthesis),
                '[' => self.add_token_type_advance(TokenType::LeftBracket),
                ']' => self.add_token_type_advance(TokenType::RightBracket),
                '{' => self.add_token_type_advance(TokenType::LeftBrace),
                '}' => self.add_token_type_advance(TokenType::RightBrace),
                ';' => self.add_token_type_advance(TokenType::Semicolon),
                ':' => self.add_token_type_advance(TokenType::Colon),
                ',' => self.add_token_type_advance(TokenType::Comma),
                '!' => self.add_token_type_advance(TokenType::Bang),
                '|' => self.add_token_type_advance(TokenType::Pipe),
                '"' => self.read_string('"'),
                '\'' => self.read_string('\''),
                _ if value.is_ascii_digit() => {
                    let number = self.read_until(|_, c| !c.is_ascii_digit());
                    let number: i32 = number.parse()?;
                    self.add_token_type(TokenType::Int(number));
                }
                _ if Self::is_reference_or_keyword(&value) => {
                    let keyword = self.read_until(|_, c| !Self::is_reference_or_keyword(c));
                    match keyword {
                        "true" => self.add_token_type(TokenType::Boolean(true)),
                        "false" => self.add_token_type(TokenType::Boolean(false)),
                        "task" => self.add_token_type(TokenType::Task),
                        "description" => self.add_token_type(TokenType::Description),
                        "variables" => self.add_token_type(TokenType::Variables),
                        "run" => self.add_token_type(TokenType::Run),
                        "include" => self.add_token_type(TokenType::Include),
                        "depends" => self.add_token_type(TokenType::Depends),
                        "condition" => self.add_token_type(TokenType::Condition),
                        "workdir" => self.add_token_type(TokenType::Workdir),
                        "of" => self.add_token_type(TokenType::Of),
                        "subtask" => self.add_token_type(TokenType::Subtask),
                        "invoked" => self.add_token_type(TokenType::Invoked),
                        "file" => {
                            let file = self.read_raw_named_body()?;
                            self.add_token_type(TokenType::File(file));
                        }
                        "macro" => {
                            let r#macro = self.read_raw_named_body()?;
                            macros.insert(r#macro.name.clone(), r#macro);
                        }
                        _ => {
                            if keyword.ends_with("!") {
                                let macro_name = keyword[0..keyword.len() - 1].to_string();
                                if let Some(macro_type) = macros.get(&macro_name) {
                                    self.apply_macro(macro_type)?;
                                    continue;
                                } else {
                                    return Err(anyhow::anyhow!(
                                        "Macro '{}' not found",
                                        macro_name
                                    ));
                                }
                            }
                            let str = keyword.to_string();
                            self.add_token_type(TokenType::Reference(str))
                        }
                    }
                }
                _ => return Err(anyhow::anyhow!("Unknown character '{}'", value)),
            }
        }
        Ok(())
    }

    fn read_string(&mut self, delim: char) {
        self.skip();
        let string = self
            .read_until(|previous, c| c == &delim && previous != Some('\\'))
            .to_string();
        self.skip();
        self.add_token_type(TokenType::String(string));
    }

    fn apply_macro(&mut self, macro_type: &RawNamedBody) -> Result<()> {
        let mut args = Vec::new();
        while self.peek_once() != Some(';') {
            if self.peek_once() == Some(' ') {
                self.skip();
            }
            let arg = self.read_until(|_, c| c == &' ' || c == &';').to_string();
            args.push(arg);
        }
        self.expect(';')?;
        let mut body = macro_type.body.clone();
        for (i, arg) in args.iter().enumerate() {
            let placeholder = format!("${}", i + 1);
            body = body.replace(&placeholder, arg);
        }
        if body.contains("$*") {
            let joined_args = args.join(" ");
            body = body.replace("$*", &joined_args);
        }
        if body.contains("$_") {
            let joined_args = args.join("_");
            body = body.replace("$_", &joined_args);
        }
        self.raw.insert_str(self.position, &body);
        Ok(())
    }

    fn read_raw_named_body(&mut self) -> Result<RawNamedBody> {
        self.expect(' ')?;
        let name = self
            .read_until(|_, c| !Self::is_reference_or_keyword(c))
            .to_string();
        self.expect(' ')?;
        self.expect('{')?;
        let mut stack = Vec::new();
        let raw_body = self.read_until(|_, c| {
            if c == &'}' {
                if stack.is_empty() {
                    return true;
                }
                stack.pop();
            } else if c == &'{' {
                stack.push(*c);
            }
            false
        });
        let non_tabbed = raw_body.replace("\n\t", "\n").replace("\n    ", "\n");
        let body = non_tabbed[1..non_tabbed.len() - 1].to_string();
        self.expect('}')?;
        Ok(RawNamedBody { name, body })
    }

    fn read_until(&mut self, mut predicate: impl FnMut(Option<char>, &char) -> bool) -> &str {
        let start = self.position;
        let mut previous = None;
        while let Some(value) = self.peek_once() {
            if predicate(previous, &value) {
                break;
            }
            previous = Some(value);
            self.skip();
        }
        &self.raw[start..self.position]
    }

    fn add_token_type_advance(&mut self, token: TokenType) {
        self.add_token(token);
        self.position += 1;
    }

    fn add_token_type(&mut self, token: TokenType) {
        self.add_token(token);
    }

    fn add_token(&mut self, token: TokenType) {
        let current_text = &self.raw[0..self.position];
        let mut row = 0;
        let mut col = 0;
        for c in current_text.chars() {
            if c == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        self.tokens.push(Token {
            token_type: token,
            position: (row, col),
        });
    }

    fn expect(&mut self, token: char) -> Result<()> {
        if let Some(value) = self.peek_once() {
            self.skip();
            if value == token {
                return Ok(());
            }
            return Err(anyhow::anyhow!(
                "Expected token {:?} but got {:?}",
                token,
                value
            ));
        }
        Err(anyhow::anyhow!("Unexpected end of input"))
    }

    fn skip(&mut self) {
        self.position += 1;
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.nth(self.position + offset)
    }

    fn peek_once(&self) -> Option<char> {
        self.peek(0)
    }

    fn peek_twice(&self) -> Option<char> {
        self.peek(1)
    }

    fn nth(&self, n: usize) -> Option<char> {
        if n < self.raw.len() {
            return Some(self.raw.as_bytes()[n] as char);
        }
        None
    }
}
