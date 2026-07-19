//! Minimal parser for Valve's text KeyValues (VDF) format, enough for
//! libraryfolders.vdf and appmanifest_*.acf. Keys are lowercased on insert.

use std::collections::HashMap;

use anyhow::{Result, bail};

#[derive(Debug)]
pub enum Value {
    Str(String),
    Obj(Obj),
}

pub type Obj = HashMap<String, Value>;

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Obj(_) => None,
        }
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            Value::Str(_) => None,
            Value::Obj(o) => Some(o),
        }
    }
}

pub fn get_str<'a>(obj: &'a Obj, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

pub fn get_obj<'a>(obj: &'a Obj, key: &str) -> Option<&'a Obj> {
    obj.get(key).and_then(Value::as_obj)
}

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '{' => tokens.push(Token::Open),
            '}' => tokens.push(Token::Close),
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => bail!("unterminated string in VDF"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(other) => s.push(other),
                            None => bail!("unterminated escape in VDF"),
                        },
                        Some(other) => s.push(other),
                    }
                }
                tokens.push(Token::Str(s));
            }
            other => {
                // Bare (unquoted) token.
                let mut s = String::from(other);
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                tokens.push(Token::Str(s));
            }
        }
    }
    Ok(tokens)
}

fn parse_obj(tokens: &[Token], pos: &mut usize) -> Result<Obj> {
    let mut obj = Obj::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Close => {
                *pos += 1;
                return Ok(obj);
            }
            Token::Str(key) => {
                let key = key.to_lowercase();
                *pos += 1;
                match tokens.get(*pos) {
                    Some(Token::Str(val)) => {
                        obj.insert(key, Value::Str(val.clone()));
                        *pos += 1;
                    }
                    Some(Token::Open) => {
                        *pos += 1;
                        let child = parse_obj(tokens, pos)?;
                        obj.insert(key, Value::Obj(child));
                    }
                    _ => bail!("expected value after key {key:?} in VDF"),
                }
            }
            Token::Open => bail!("unexpected '{{' in VDF"),
        }
    }
    Ok(obj)
}

/// Parse a VDF document into its top-level key/value pairs.
pub fn parse(text: &str) -> Result<Obj> {
    let tokens = tokenize(text)?;
    let mut pos = 0;
    parse_obj(&tokens, &mut pos)
}
