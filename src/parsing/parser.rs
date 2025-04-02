use crate::parsing::tokenizer::Tokenizer;
use crate::parsing::tokenizer::{Token, TokenType};
use crate::tasks::condition::{Condition, ConditionType};
use crate::tasks::environment::{Environment, Value};
use crate::tasks::run::Run;
use crate::tasks::task::Task;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Parser {
    tokens: Vec<Token>,
    position: usize,
    pub(crate) conditions: Vec<Condition>,
    pub(crate) tasks: HashMap<String, Task>,
    pub(crate) subtasks: HashMap<String, Task>,
}

impl Parser {
    pub fn from_file(path: &str) -> Result<Self> {
        let mut tokenizer = Tokenizer::from_file(path)?;
        tokenizer.run()?;
        Ok(Self::new(tokenizer))
    }

    pub fn new(tokenizer: Tokenizer) -> Self {
        Parser {
            tokens: tokenizer.tokens,
            position: 0,
            conditions: Vec::new(),
            tasks: HashMap::new(),
            subtasks: HashMap::new(),
        }
    }

    pub fn run(
        &mut self,
        global_env: Arc<Environment>,
    ) -> Result<()> {
        while self.has_next() {
            let token = self.next()?;
            match &token.token_type {
                TokenType::Include => {
                    let path = self.expect_str("include file path")?;
                    let mut included_parser = Parser::from_file(&path)?;
                    included_parser.run(global_env.clone())?;
                    self.tasks.extend(included_parser.tasks);
                    self.subtasks.extend(included_parser.subtasks);
                    self.conditions.extend(included_parser.conditions);
                    self.expect(TokenType::Semicolon)?;
                }
                TokenType::Task => {
                    let task = self.parse_task(global_env.clone())?;
                    self.tasks.insert(task.name.clone(), task);
                }
                TokenType::Subtask => {
                    let task = self.parse_task(global_env.clone())?;
                    self.subtasks.insert(task.name.clone(), task);
                }
                TokenType::Condition => {
                    let local_conditions = self.parse_opt_array(Self::parse_condition)?;
                    self.conditions.extend(local_conditions);
                    self.consume_opt(TokenType::Semicolon);
                }
                TokenType::Variables => {
                    let variables = self.parse_opt_array(Self::parse_variable)?;
                    for (name, value) in variables {
                        global_env.add(name, value);
                    }
                    self.consume_opt(TokenType::Semicolon);
                }
                TokenType::File(file) => {
                    let name = file.name.to_string();
                    if global_env.has(&name) {
                        return Err(anyhow::anyhow!("file {name} has already been defined",))
                    }
                    global_env.add(name, Value::String(file.body.to_string()));
                }
                _ => return Err(anyhow::anyhow!("Unexpected token: {token}",)),
            }
        }

        Ok(())
    }

    fn parse_task(&mut self, parent_env: Arc<Environment>) -> Result<Task> {
        let name = self.expect_reference("task name")?;
        let mut task = Task::new(name, Environment::derive(parent_env));

        if self.peek() == Some(&TokenType::Of) {
            self.skip();
            task.parent_id = Some(self.expect_reference("parent task id")?);
        }

        self.expect(TokenType::LeftBrace)?;

        loop {
            let token = self.next()?;
            match &token.token_type {
                TokenType::Description => {
                    let description = self.expect_str("task description")?;
                    task.description = Some(Value::String(description));
                    self.expect(TokenType::Semicolon)?;
                }
                TokenType::Variables => {
                    let variables = self.parse_opt_array(Self::parse_variable)?;
                    for (name, value) in variables {
                        task.environment.add(name, value);
                    }
                    self.consume_opt(TokenType::Semicolon);
                }
                TokenType::Depends => {
                    let dependencies = self.parse_opt_array(|parser| {
                        parser.expect_reference("task dependency name")
                    })?;
                    task.depends.extend(dependencies);
                    self.consume_opt(TokenType::Semicolon);
                }
                TokenType::Run => {
                    let commands = self.parse_opt_array(|parser| parser.parse_command(false))?;
                    task.run.extend(commands);
                    self.consume_opt(TokenType::Semicolon);
                }
                TokenType::Condition => {
                    let conditions = self.parse_opt_array(Self::parse_condition)?;
                    task.conditions.extend(conditions);
                    self.consume_opt(TokenType::Semicolon);
                }
                TokenType::Workdir => {
                    let workdir = self.expect_str("workdir path")?;
                    task.workdir = Some(Value::String(workdir));
                    self.expect(TokenType::Semicolon)?;
                }
                TokenType::RightBrace => break,
                _ => return Err(anyhow::anyhow!("Unexpected token in tasks {token}")),
            }
        }
        Ok(task)
    }

    fn parse_condition(&mut self) -> Result<Condition> {
        let condition = self.expect_reference("condition type")?;
        let invert = match self.peek() {
            Some(TokenType::Bang) => {
                self.skip();
                true
            },
            _ => false,
        };
        let value = match condition.as_str() {
            "exists" => {
                let path = self.expect_str("file or directory path")?;
                let value = Value::String(path);
                Condition { condition_type: ConditionType::Exists(value), invert }
            }
            "shell" => {
                let command = self.parse_command(true)?;
                Condition { condition_type: ConditionType::Shell(command), invert }
            }
            "ask" => {
                let prompt = self.expect_str("ask prompt")?;
                Condition {
                    condition_type: ConditionType::Ask(Value::String(prompt)),
                    invert,
                }
            }
            _ => return Err(anyhow::anyhow!("Unknown condition type {condition}")),
        };
        Ok(value)
    }

    fn parse_command(&mut self, silent: bool) -> Result<Run> {
        let mut parts = Vec::new();
        while let Some(token) = self.peek() {
            match token {
                TokenType::Reference(name) => parts.push(Value::String(name.clone())),
                TokenType::String(value) => parts.push(Value::String(value.clone())),
                TokenType::Int(value) => parts.push(Value::Int(*value)),
                TokenType::Boolean(value) => parts.push(Value::Boolean(*value)),
                _ => break,
            }
            self.skip();
        }
        let mut sub_run = None;
        if self.peek() == Some(&TokenType::Pipe) {
            self.skip();
            let run = self.parse_command(silent)?;
            sub_run = Some(Box::from(run));
        }
        Ok(Run {
            parts,
            sub_run,
            silent,
        })
    }

    fn parse_variable(&mut self) -> Result<(String, Value)> {
        let name = self.expect_str("variable name")?;
        self.expect(TokenType::Colon)?;
        let token = self.next()?;
        let value = match &token.token_type {
            TokenType::String(value) => Value::String(value.clone()),
            TokenType::Int(value) => Value::Int(*value),
            TokenType::Boolean(value) => Value::Boolean(*value),
            TokenType::Reference(value) => match value.as_str() {
                "env" => {
                    self.expect(TokenType::LeftParenthesis)?;
                    let str = self.expect_str("env variable name")?;
                    self.expect(TokenType::RightParenthesis)?;
                    Value::Env(str)
                }
                _ => return Err(anyhow::anyhow!("Unexpected variable reference {token}")),
            },
            _ => {
                return Err(anyhow::anyhow!(
                    "Expected a value type for variable but got {token}"
                ));
            }
        };
        Ok((name, value))
    }

    fn parse_opt_array<T>(
        &mut self,
        mut parse_fn: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        if self.peek() == Some(&TokenType::LeftBracket) {
            let result = self.parse_array(parse_fn)?;
            return Ok(result);
        }
        Ok(vec![parse_fn(self)?])
    }

    fn parse_array<T>(
        &mut self,
        mut parse_fn: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let mut result = Vec::new();
        self.expect(TokenType::LeftBracket)?;
        while let Some(token) = self.peek() {
            if token == &TokenType::RightBracket {
                self.skip();
                break;
            }
            let value = parse_fn(self)?;
            result.push(value);
            if self.peek() == Some(&TokenType::Comma) {
                self.skip();
            }
        }
        Ok(result)
    }

    fn expect_str(&mut self, err_msg: &str) -> Result<String> {
        let token = self.next()?;
        if let TokenType::String(value) = &token.token_type {
            return Ok(value.clone());
        }
        Err(anyhow::anyhow!("Expected {err_msg} but got {token}"))
    }

    fn expect_reference(&mut self, err_msg: &str) -> Result<String> {
        let token = self.next()?;
        if let TokenType::Reference(value) = &token.token_type {
            return Ok(value.clone());
        }
        Err(anyhow::anyhow!("Expected {err_msg} but got {token}"))
    }

    fn expect(&mut self, token_type: TokenType) -> Result<()> {
        let token = self.next()?;
        if token.token_type == token_type {
            return Ok(());
        }
        Err(anyhow::anyhow!("Expected {:?} but got {token}", token_type))
    }

    fn consume_opt(&mut self, token: TokenType) {
        if let Some(value) = self.peek() {
            if value == &token {
                self.skip();
            }
        }
    }

    fn skip(&mut self) {
        self.position += 1;
    }

    fn peek(&self) -> Option<&TokenType> {
        self.nth(self.position).map(|token| &token.token_type)
    }

    fn next(&mut self) -> Result<&Token> {
        let pos = self.position;
        if self.has_next() {
            self.position = pos + 1;
        }
        self.nth(pos)
            .ok_or(anyhow::anyhow!("Unexpected end of input"))
    }

    fn nth(&self, n: usize) -> Option<&Token> {
        if n < self.tokens.len() {
            return Some(&self.tokens[n]);
        }
        None
    }

    fn has_next(&self) -> bool {
        self.position < self.tokens.len()
    }
}
