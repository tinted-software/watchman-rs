//! Shared dynamic value type used for both BSER and JSON encoding, mirroring the
//! subset of watchman's PDU value model that we need.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    Array(Vec<Value>),
    /// Ordered map (insertion order preserved), matching watchman's object encoding.
    Object(Vec<(String, Value)>),
}

#[allow(dead_code)]
impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    pub fn obj() -> ObjBuilder {
        ObjBuilder(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Double(d) => Some(*d as i64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}

/// Small helper for building `Value::Object` fluently.
pub struct ObjBuilder(Vec<(String, Value)>);

impl ObjBuilder {
    pub fn set(mut self, key: &str, val: impl Into<Value>) -> Self {
        self.0.push((key.to_string(), val.into()));
        self
    }

    pub fn build(self) -> Value {
        Value::Object(self.0)
    }
}
