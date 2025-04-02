use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub enum Value {
    /// String value
    String(String),
    /// Integer value
    Int(i32),
    /// Boolean value
    Boolean(bool),
    /// Environment variable
    Env(String),
}

lazy_static! {
    /// Regex to match variable names
    static ref TEMPLATING_REGEX: Regex = Regex::new(r"(\{\{\s?([\w\.\-\/]+)\s}})").unwrap();
}

impl Value {
    pub fn as_string(&self, env: &Environment) -> Result<String> {
        match self {
            Value::String(s) => Self::template_str(s, env),
            Value::Int(i) => Ok(i.to_string()),
            Value::Boolean(b) => Ok(b.to_string()),
            Value::Env(name) => std::env::var(name)
                .map_err(|_| anyhow::anyhow!("Environment variable {name} not found")),
        }
    }

    /// Resolve templating in the string
    /// Templates are in the form of {{ variable_name }}
    pub fn template_str(raw: &str, env: &Environment) -> Result<String> {
        let mut result = raw.to_string();
        for (_, [literal, name]) in TEMPLATING_REGEX.captures_iter(raw).map(|c| c.extract()) {
            let value = env
                .find(name)
                .ok_or_else(|| anyhow::anyhow!("Variable {} not found in environment", name))?;
            let value_str = value.as_string(env)?;
            result = result.replace(literal, &value_str);
        }
        Ok(result)
    }
}

#[derive(Debug)]
pub struct Environment {
    /// Environment variables
    pub variables: Mutex<HashMap<String, Arc<Value>>>,
    /// Reference to a parent environment
    pub parent: Option<Arc<Environment>>,
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

impl Environment {
    pub fn find(&self, name: &str) -> Option<Arc<Value>> {
        // Check the current environment
        let variables = self.variables.lock().unwrap();
        if let Some(value) = variables.get(name) {
            return Some(value.clone());
        }
        // Check the parent environment if it exists
        if let Some(parent) = &self.parent {
            return parent.find(name);
        }
        None
    }

    pub fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    pub fn add(&self, name: String, value: Value) {
        let mut variables = self.variables.lock().unwrap();
        variables.insert(name, Arc::new(value));
    }

    pub fn new() -> Self {
        Environment {
            variables: Mutex::new(HashMap::new()),
            parent: None,
        }
    }

    pub fn derive(parent: Arc<Environment>) -> Environment {
        Environment {
            variables: Mutex::new(HashMap::new()),
            parent: Some(parent),
        }
    }
}
