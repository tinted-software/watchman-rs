//! A subset of watchman's JSON expression query evaluator, covering the
//! terms used by `watchman_client`/buck2:
//! bare `"true"`/`"false"`/`"exists"`/`"empty"`, and array forms
//! `["allof", ...]`, `["anyof", ...]`, `["not", expr]`,
//! `["match", glob]`/`["imatch", glob]`, `["suffix", ext-or-list]`,
//! `["type", code]`, `["name", name-or-list]`.

use crate::tree::{FileInfo, Kind};
use crate::value::Value;

pub enum Expr {
    True,
    False,
    Exists,
    Empty,
    Match(String),
    IMatch(String),
    Suffix(Vec<String>),
    Type(String),
    Name(Vec<String>),
    Not(Box<Expr>),
    AllOf(Vec<Expr>),
    AnyOf(Vec<Expr>),
}

impl Expr {
    pub fn parse(v: &Value) -> Result<Expr, String> {
        // Bare string shorthand: "true", "false", "exists", "empty".
        if let Some(s) = v.as_str() {
            return match s {
                "true" => Ok(Expr::True),
                "false" => Ok(Expr::False),
                "exists" => Ok(Expr::Exists),
                "empty" => Ok(Expr::Empty),
                other => Err(format!("unsupported bare expression term: {other}")),
            };
        }

        let arr = v
            .as_array()
            .ok_or("expression must be an array or bare term string")?;
        let op = arr
            .first()
            .and_then(|v| v.as_str())
            .ok_or("expression missing op")?;
        Ok(match op {
            "true" => Expr::True,
            "false" => Expr::False,
            "exists" => Expr::Exists,
            "empty" => Expr::Empty,
            "match" | "glob" => {
                let pat = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or("match needs a pattern")?;
                Expr::Match(pat.to_string())
            }
            "imatch" => {
                let pat = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or("imatch needs a pattern")?;
                Expr::IMatch(pat.to_lowercase())
            }
            "suffix" => {
                let val = arr.get(1).ok_or("suffix needs a value")?;
                Expr::Suffix(string_or_list(val)?)
            }
            "type" => {
                let code = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or("type needs a code")?;
                Expr::Type(code.to_string())
            }
            "name" => {
                let val = arr.get(1).ok_or("name needs a value")?;
                Expr::Name(string_or_list(val)?)
            }
            "not" => {
                let inner = arr.get(1).ok_or("not needs an expression")?;
                Expr::Not(Box::new(Expr::parse(inner)?))
            }
            "allof" => Expr::AllOf(
                arr[1..]
                    .iter()
                    .map(Expr::parse)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            "anyof" => Expr::AnyOf(
                arr[1..]
                    .iter()
                    .map(Expr::parse)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            other => return Err(format!("unsupported expression term: {other}")),
        })
    }

    pub fn eval(&self, f: &FileInfo) -> bool {
        match self {
            Expr::True => true,
            Expr::False => false,
            Expr::Exists => f.exists,
            Expr::Empty => {
                f.exists && f.size == 0 && matches!(f.kind, Kind::Regular | Kind::Directory)
            }
            Expr::Match(pat) => glob_match(pat, basename(&f.name)),
            Expr::IMatch(pat) => glob_match(pat, &basename(&f.name).to_lowercase()),
            Expr::Suffix(sufs) => {
                let ext = f.name.rsplit('.').next().unwrap_or("");
                sufs.iter().any(|s| s.eq_ignore_ascii_case(ext))
            }
            Expr::Type(code) => f.kind.code() == code,
            Expr::Name(names) => names.iter().any(|n| n == basename(&f.name)),
            Expr::Not(e) => !e.eval(f),
            Expr::AllOf(items) => items.iter().all(|e| e.eval(f)),
            Expr::AnyOf(items) => items.iter().any(|e| e.eval(f)),
        }
    }
}

fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn string_or_list(v: &Value) -> Result<Vec<String>, String> {
    if let Some(s) = v.as_str() {
        return Ok(vec![s.to_string()]);
    }
    if let Some(items) = v.as_array() {
        return Ok(items
            .iter()
            .filter_map(|i| i.as_str().map(|s| s.to_string()))
            .collect());
    }
    Err("expected a string or list of strings".to_string())
}

/// Minimal shell-style glob matcher supporting `*`, `?`, and `**` (treated as `*`).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => {
            let mut rest = p;
            while rest.first() == Some(&'*') {
                rest = &rest[1..];
            }
            if rest.is_empty() {
                return true;
            }
            for i in 0..=t.len() {
                if glob_rec(rest, &t[i..]) {
                    return true;
                }
            }
            false
        }
        Some('?') => !t.is_empty() && glob_rec(&p[1..], &t[1..]),
        Some(c) => !t.is_empty() && t[0] == *c && glob_rec(&p[1..], &t[1..]),
    }
}
